use std::os::unix::io::{FromRawFd, IntoRawFd};

use anyhow::Context;

use crate::protocol::messages::{ClientMessage, DaemonMessage};
use crate::client;

/// Attach to a named session.
pub fn do_attach(name: &str) -> anyhow::Result<()> {
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
pub fn do_follow(name: &str, plain: bool) -> anyhow::Result<()> {
    use crate::protocol::codec::{try_read_frame_async, write_frame, write_frame_async};
    use crate::util::{clean_control_chars, strip_ansi};
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
                            if plain {
                                let cleaned = clean_control_chars(&strip_ansi(&data));
                                // Skip empty chunks that result from stripping
                                if !cleaned.is_empty() {
                                    let _ = stdout.write_all(&cleaned);
                                    let _ = stdout.flush();
                                }
                            } else {
                                let _ = stdout.write_all(&data);
                                let _ = stdout.flush();
                            }
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

#[cfg(test)]
mod tests {
    use crate::protocol::codec::{try_read_frame_async, write_frame_async};
    use crate::protocol::messages::{CaptureMode, ClientMessage, DaemonMessage};
    use crate::util::strip_ansi;

    /// Integration test: verify AttachInput reaches the child process via daemon.
    #[tokio::test]
    async fn test_attach_input_reaches_session() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-{}", std::process::id()));
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

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

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

        // === Test 1: AttachInput with \n ===
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

        // === Test 2: AttachInput with \r ===
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

        let _ = write_frame_async(&mut writer, &ClientMessage::Detach).await;
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: AttachInput with individual keystrokes.
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

        // Send individual characters like crossterm would.
        for &byte in b"hello" {
            write_frame_async(
                &mut writer,
                &ClientMessage::AttachInput(vec![byte]),
            )
            .await
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        write_frame_async(
            &mut writer,
            &ClientMessage::AttachInput(vec![b'\r']),
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

    /// Regression test: tokio::net::UnixStream::from_std requires O_NONBLOCK.
    #[tokio::test]
    async fn test_unix_stream_needs_nonblocking_for_tokio() {
        use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

        let dir = std::env::temp_dir().join(format!("amux-test-nb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);

        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        let std_stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();

        let fd = std_stream.as_raw_fd();
        let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).unwrap();
        let oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
        assert!(
            !oflags.contains(nix::fcntl::OFlag::O_NONBLOCK),
            "std socket should start blocking"
        );

        let mut new_flags = oflags;
        new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
        nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(new_flags)).unwrap();

        let raw_fd = std_stream.into_raw_fd();
        let rebuilt = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
        let tokio_stream = tokio::net::UnixStream::from_std(rebuilt);
        assert!(tokio_stream.is_ok(), "from_std must succeed with O_NONBLOCK set");

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reproduce the exact do_attach code path: sync write Attach, then convert
    /// the socket to async and read Output frames.
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

        let std_sock_path = sock_path.clone();
        let std_stream =
            std::os::unix::net::UnixStream::connect(&std_sock_path).unwrap();

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

    /// Test that follow --plain strips ANSI from streamed output.
    #[test]
    fn test_follow_plain_strips_ansi() {
        use crate::util::{clean_control_chars, strip_ansi};
        // Simulate what do_follow does in plain mode: strip_ansi then clean_control_chars
        let ansi_data = b"\x1b[32mhello\x1b[0m world\r\n";
        let cleaned = clean_control_chars(&strip_ansi(ansi_data));
        let output = String::from_utf8_lossy(&cleaned);
        assert!(output.contains("hello world"), "plain output should contain 'hello world', got: {:?}", output);
        // Should not contain ESC
        assert!(!cleaned.contains(&0x1b), "plain output should not contain ESC bytes");
    }

    /// Test that follow defaults to plain (no --raw flag).
    #[test]
    fn test_follow_defaults_to_plain() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["amux", "follow", "-t", "mysession"]).unwrap();
        match cli.command.unwrap() {
            crate::cli::Command::Follow { name, raw, .. } => {
                assert_eq!(name, "mysession");
                assert!(!raw, "follow should default to plain (raw=false)");
            }
            _ => panic!("expected Follow command"),
        }
    }

    /// Test that follow --raw flag is recognized.
    #[test]
    fn test_follow_raw_cli_flag() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["amux", "follow", "-t", "mysession", "--raw"]).unwrap();
        match cli.command.unwrap() {
            crate::cli::Command::Follow { name, raw, .. } => {
                assert_eq!(name, "mysession");
                assert!(raw);
            }
            _ => panic!("expected Follow command"),
        }
    }

    /// Test that follow --plain is still accepted (backwards compat, no-op).
    #[test]
    fn test_follow_plain_cli_flag_compat() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["amux", "follow", "-t", "mysession", "--plain"]).unwrap();
        match cli.command.unwrap() {
            crate::cli::Command::Follow { name, raw, .. } => {
                assert_eq!(name, "mysession");
                assert!(!raw, "--plain should not set raw");
            }
            _ => panic!("expected Follow command"),
        }
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

    /// Bd-is4: bumping `attach_count` while attached lets `amux top`
    /// detect that an interactive client owns the size — top defers to
    /// the attacher and does not send `ResizeSession`. Detach must
    /// decrement so top regains size control.
    #[tokio::test]
    async fn test_attach_count_tracked_in_session_info() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-attach-count-{}", std::process::id()));
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

        // Helper to fetch SessionInfo via GetSessionInfo.
        async fn get_attach_count(sock_path: &std::path::Path, name: &str) -> u32 {
            let stream = tokio::net::UnixStream::connect(sock_path).await.unwrap();
            let (mut r, mut w) = stream.into_split();
            write_frame_async(
                &mut w,
                &ClientMessage::GetSessionInfo { name: name.to_string() },
            )
            .await
            .unwrap();
            let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
            match resp {
                DaemonMessage::SessionDetail(info) => info.attach_count,
                other => panic!("expected SessionDetail, got {:?}", other),
            }
        }

        // Create a session.
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut r, mut w) = stream.into_split();
        write_frame_async(
            &mut w,
            &ClientMessage::CreateSession {
                name: Some("ac".to_string()),
                command: vec!["cat".to_string()],
                env: None,
                cwd: None,
                cols: Some(80),
                rows: Some(24),
            },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));
        drop(r);
        drop(w);

        assert_eq!(get_attach_count(&sock_path, "ac").await, 0);

        // Attach in a separate connection.
        let attach_stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut ar, mut aw) = attach_stream.into_split();
        write_frame_async(
            &mut aw,
            &ClientMessage::Attach {
                name: "ac".to_string(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();
        // Wait briefly for the attach to register.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(get_attach_count(&sock_path, "ac").await, 1);

        // Detach. The reader_task tear-down decrements attach_count.
        let _ = write_frame_async(&mut aw, &ClientMessage::Detach).await;
        // Drain any pending output the daemon flushed before noticing detach.
        let _ = try_read_frame_async::<DaemonMessage>(&mut ar).await;
        // Allow the daemon side to finish the loop and decrement.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(get_attach_count(&sock_path, "ac").await, 0);

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bd-is4: ResizeSession is the stateless one-shot resize used by
    /// `amux top` to match the agent's PTY to the viewer's terminal. It
    /// must drive TIOCSWINSZ exactly like AttachResize does.
    #[tokio::test]
    async fn test_resize_session_changes_pty_size() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-rsz-sess-{}", std::process::id()));
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

        // Spawn a session that prints stty size on a loop so we can detect
        // when the resize landed.
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut r, mut w) = stream.into_split();
        write_frame_async(
            &mut w,
            &ClientMessage::CreateSession {
                name: Some("rsz".to_string()),
                command: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "while :; do stty size; sleep 0.2; done".to_string(),
                ],
                env: None,
                cwd: None,
                cols: Some(80),
                rows: Some(24),
            },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));
        drop(r);
        drop(w);

        // ResizeSession to 100x80.
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut r, mut w) = stream.into_split();
        write_frame_async(
            &mut w,
            &ClientMessage::ResizeSession {
                name: "rsz".to_string(),
                cols: 100,
                rows: 80,
            },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::Ok), "got {:?}", resp);
        drop(r);
        drop(w);

        // Capture scrollback (raw) and verify "80 100" appears.
        let mut saw_new_size = false;
        for _ in 0..15 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
            let (mut r, mut w) = stream.into_split();
            write_frame_async(
                &mut w,
                &ClientMessage::CaptureScrollback {
                    name: "rsz".to_string(),
                    lines: 50,
                    mode: CaptureMode::Raw,
                },
            )
            .await
            .unwrap();
            let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
            if let DaemonMessage::CaptureOutput(data) = resp {
                let plain = strip_ansi(&data);
                let s = String::from_utf8_lossy(&plain);
                if s.contains("80 100") {
                    saw_new_size = true;
                    break;
                }
            }
        }
        assert!(saw_new_size, "ResizeSession did not change PTY size");

        // SessionInfo should report the new rows/cols too.
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut r, mut w) = stream.into_split();
        write_frame_async(
            &mut w,
            &ClientMessage::GetSessionInfo { name: "rsz".to_string() },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        match resp {
            DaemonMessage::SessionDetail(info) => {
                assert_eq!(info.rows, 80);
                assert_eq!(info.cols, 100);
                assert_eq!(info.attach_count, 0);
            }
            other => panic!("expected SessionDetail, got {:?}", other),
        }

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bd-is4: ResizeSession on a missing session returns an error rather
    /// than silently dropping the request.
    #[tokio::test]
    async fn test_resize_session_missing_returns_error() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-rsz-miss-{}", std::process::id()));
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
        write_frame_async(
            &mut w,
            &ClientMessage::ResizeSession {
                name: "nope".to_string(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(
            matches!(resp, DaemonMessage::Error(ref e) if e.contains("not found")),
            "got {:?}",
            resp
        );

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end check that the existing AttachResize plumbing actually
    /// reshapes the agent's PTY. The bd-is4 design relies on this: a
    /// detached session spawns at 80x60 by default, and attaching from a
    /// larger/smaller terminal is supposed to resize the PTY so the agent
    /// redraws to fit. If this regressed, agents would be stuck at the
    /// spawn-time canvas and `amux attach` would feel cramped or scroll.
    #[tokio::test]
    async fn test_attach_resize_changes_pty_size() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-resize-{}", std::process::id()));
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

        // Spawn a loop that prints stty size every 200ms so we can observe
        // the size change without timing the resize precisely.
        write_frame_async(
            &mut w,
            &ClientMessage::CreateSession {
                name: Some("resize-test".to_string()),
                command: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "while :; do stty size; sleep 0.2; done".to_string(),
                ],
                env: None,
                cwd: None,
                cols: Some(80),
                rows: Some(60),
            },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));
        drop(r);
        drop(w);

        // Open a fresh connection for the attach (the daemon takes ownership
        // of the conn for streaming).
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        write_frame_async(
            &mut writer,
            &ClientMessage::Attach {
                name: "resize-test".to_string(),
                cols: 80,
                rows: 60,
            },
        )
        .await
        .unwrap();

        // Wait until we see "60 80" (initial size) at least once.
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            output.extend_from_slice(&data);
                            let plain = strip_ansi(&output);
                            let s = String::from_utf8_lossy(&plain);
                            if s.contains("60 80") { break; }
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timeout waiting for initial size '60 80', got: {:?}", String::from_utf8_lossy(&output));
                }
            }
        }

        // Send AttachResize to 100 cols x 40 rows.
        write_frame_async(
            &mut writer,
            &ClientMessage::AttachResize { cols: 100, rows: 40 },
        )
        .await
        .unwrap();

        // Wait for "40 100" to appear in the output stream.
        output.clear();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw_new_size = false;
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            output.extend_from_slice(&data);
                            let plain = strip_ansi(&output);
                            let s = String::from_utf8_lossy(&plain);
                            if s.contains("40 100") {
                                saw_new_size = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        assert!(
            saw_new_size,
            "AttachResize did not resize PTY: never saw '40 100' in stty output. Got: {:?}",
            String::from_utf8_lossy(&output)
        );

        let _ = write_frame_async(&mut writer, &ClientMessage::Detach).await;
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
