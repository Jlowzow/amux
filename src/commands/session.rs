use std::collections::HashMap;

use crate::cli::DEFAULT_DETACHED_ROWS;
use crate::protocol::messages::{CaptureMode, ClientMessage, DaemonMessage};
use crate::util::{create_git_worktree, ensure_daemon_running, parse_env_vars};
use crate::client;

use super::attach::do_attach;

pub fn new_session(
    name: Option<String>,
    detached: bool,
    env: Vec<String>,
    cwd: Option<String>,
    worktree: Option<String>,
    init_message: Option<String>,
    rows: Option<u16>,
    cmd: Vec<String>,
) -> anyhow::Result<()> {
    if std::env::var("AMUX_DEBUG").is_ok() {
        eprintln!("amux-debug: ensure_daemon_running");
    }
    ensure_daemon_running()?;
    if std::env::var("AMUX_DEBUG").is_ok() {
        eprintln!("amux-debug: daemon running, parsing env");
    }
    let mut env_map = parse_env_vars(&env)?;

    // Handle --worktree: create a git worktree, set cwd to it, store metadata.
    let cwd = if let Some(ref branch) = worktree {
        let worktree_path = create_git_worktree(branch)?;
        let env = env_map.get_or_insert_with(HashMap::new);
        env.insert("AMUX_WORKTREE_PATH".to_string(), worktree_path.clone());
        env.insert("AMUX_WORKTREE_BRANCH".to_string(), branch.clone());
        Some(worktree_path)
    } else {
        cwd
    };
    // --init-message implies --detached
    let detached = detached || init_message.is_some();

    if detached {
        // Detached sessions default to 80x60 so alt-screen TUIs (claude,
        // vim) running inside them have enough vertical space for `amux
        // top` previews. Without this, the daemon's PTY would size to
        // 80x24 and the agent's rendered frame would only fill 22 rows of
        // even a tall (60-row) preview pane. See bd-is4. The user can
        // override with `--rows`; the AttachResize plumbing still resizes
        // the PTY to the attaching terminal's actual size at attach time.
        let resp = client::request(&ClientMessage::CreateSession {
            name,
            command: cmd,
            env: env_map,
            cwd: cwd.clone(),
            cols: None,
            rows: Some(rows.unwrap_or(DEFAULT_DETACHED_ROWS)),
        })?;
        let session_name = match resp {
            DaemonMessage::SessionCreated { name } => {
                eprintln!("amux: created session '{}'", name);
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

        if let Some(msg) = init_message {
            // Wait for the session to produce some output (indicating readiness)
            wait_for_session_ready(&session_name)?;
            send_keys(&session_name, false, &[msg])?;
        }
    } else {
        // Create then attach.
        if std::env::var("AMUX_DEBUG").is_ok() {
            eprintln!("amux-debug: creating session (non-detached)");
        }
        // Non-detached: spawn at the user's terminal size by default so the
        // attached view matches the agent's canvas exactly. An explicit
        // --rows wins (less common — typically used to fix a session at a
        // larger size than the current window), but the attaching terminal
        // would override the height again via AttachResize, so honoring the
        // flag here is mostly cosmetic.
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let spawn_rows = rows.unwrap_or(term_rows);
        let resp = client::request(&ClientMessage::CreateSession {
            name: name.clone(),
            command: cmd,
            env: env_map,
            cwd,
            cols: Some(term_cols),
            rows: Some(spawn_rows),
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
    Ok(())
}

/// Wait for a session to produce output, indicating it is ready for input.
/// Polls the scrollback up to 50 times (5 seconds total) waiting for non-empty output.
fn wait_for_session_ready(name: &str) -> anyhow::Result<()> {
    for _ in 0..50 {
        let resp = client::request(&ClientMessage::CaptureScrollback {
            name: name.to_string(),
            lines: 1,
            mode: CaptureMode::Plain,
        })?;
        match resp {
            DaemonMessage::CaptureOutput(data) if !data.is_empty() => {
                return Ok(());
            }
            _ => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    // Proceed anyway after timeout - the session may just not produce output before input
    Ok(())
}

pub fn do_kill_all() -> anyhow::Result<()> {
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

pub fn send_keys(name: &str, literal: bool, text: &[String]) -> anyhow::Result<()> {
    ensure_daemon_running()?;
    let joined = text.join(" ");
    let needs_enter = !literal;
    let resp = client::request(&ClientMessage::SendInput {
        name: name.to_string(),
        data: joined.into_bytes(),
        newline: false,
    })?;
    match resp {
        DaemonMessage::InputSent => {}
        DaemonMessage::Error(e) => {
            eprintln!("amux: error: {}", e);
            std::process::exit(1);
        }
        other => eprintln!("amux: unexpected: {:?}", other),
    }
    if needs_enter {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let resp = client::request(&ClientMessage::SendInput {
            name: name.to_string(),
            data: vec![b'\r'],
            newline: false,
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
    Ok(())
}

pub fn has_session(name: &str) -> anyhow::Result<()> {
    ensure_daemon_running()?;
    let resp = client::request(&ClientMessage::HasSession {
        name: name.to_string(),
    });
    match resp {
        Ok(DaemonMessage::SessionExists(true)) => {
            std::process::exit(0);
        }
        _ => {
            std::process::exit(1);
        }
    }
}

pub fn capture_scrollback(name: &str, lines: usize, plain: bool) -> anyhow::Result<()> {
    ensure_daemon_running()?;
    // Plain mode (the default) asks the daemon for the rendered virtual
    // terminal screen, which correctly handles TUI apps that redraw with
    // cursor-addressed escape sequences. Raw mode streams the PTY bytes
    // verbatim so callers can re-render them in their own terminal.
    let mode = if plain { CaptureMode::Plain } else { CaptureMode::Raw };
    let resp = client::request(&ClientMessage::CaptureScrollback {
        name: name.to_string(),
        lines,
        mode,
    })?;
    match resp {
        DaemonMessage::CaptureOutput(data) => {
            use std::io::Write;
            // Plain mode is already rendered by the daemon. Raw mode
            // passes bytes through untouched.
            std::io::stdout().write_all(&data)?;
            // The rendered screen has no trailing newline; add one for
            // nicer shell output so the prompt lands on a fresh line.
            if plain && !data.ends_with(b"\n") {
                std::io::stdout().write_all(b"\n")?;
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

#[cfg(test)]
mod tests {
    use crate::protocol::codec::{try_read_frame_async, write_frame_async};
    use crate::protocol::messages::{CaptureMode, ClientMessage, DaemonMessage};
    use crate::util::strip_ansi;

    /// Integration test: verify SendInput path works.
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

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        write_frame_async(
            &mut writer,
            &ClientMessage::CaptureScrollback {
                name: "send-test".to_string(),
                lines: 10,
                mode: CaptureMode::Raw,
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

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: CaptureScrollback on an active session returns its output.
    #[tokio::test]
    async fn test_capture_active_session() {
        use tokio::sync::broadcast;

        let dir =
            std::env::temp_dir().join(format!("amux-test-cap-active-{}", std::process::id()));
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
                name: Some("cap-active".to_string()),
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

        write_frame_async(
            &mut writer,
            &ClientMessage::SendInput {
                name: "cap-active".to_string(),
                data: b"capture-test-data".to_vec(),
                newline: true,
            },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::InputSent));

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        write_frame_async(
            &mut writer,
            &ClientMessage::CaptureScrollback {
                name: "cap-active".to_string(),
                lines: 50,
                mode: CaptureMode::Raw,
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
                    output_str.contains("capture-test-data"),
                    "expected 'capture-test-data' in capture output, got: {:?}",
                    output_str
                );
            }
            other => panic!("expected CaptureOutput, got {:?}", other),
        }

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: CaptureScrollback on a dead session still returns its scrollback.
    #[tokio::test]
    async fn test_capture_dead_session() {
        use tokio::sync::broadcast;

        let dir =
            std::env::temp_dir().join(format!("amux-test-cap-dead-{}", std::process::id()));
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
                name: Some("cap-dead".to_string()),
                command: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo dead-session-output; exit 0".to_string(),
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

        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        write_frame_async(
            &mut writer,
            &ClientMessage::CaptureScrollback {
                name: "cap-dead".to_string(),
                lines: 50,
                mode: CaptureMode::Raw,
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
                    output_str.contains("dead-session-output"),
                    "expected 'dead-session-output' in capture output, got: {:?}",
                    output_str
                );
            }
            other => panic!("expected CaptureOutput, got {:?}", other),
        }

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: CaptureScrollback on a nonexistent session returns an error.
    #[tokio::test]
    async fn test_capture_nonexistent_session() {
        use tokio::sync::broadcast;

        let dir =
            std::env::temp_dir().join(format!("amux-test-cap-ne-{}", std::process::id()));
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
            &ClientMessage::CaptureScrollback {
                name: "nonexistent".to_string(),
                lines: 10,
                mode: CaptureMode::Plain,
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
}
