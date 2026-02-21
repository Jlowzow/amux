use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};

use crate::daemon::registry::Registry;
use crate::protocol::codec::{try_read_frame_async, write_frame_async};
use crate::protocol::messages::{ClientMessage, DaemonMessage};

pub async fn run_server(listener: UnixListener, shutdown_tx: broadcast::Sender<()>) {
    let registry = Arc::new(Mutex::new(Registry::new()));
    let mut shutdown_rx = shutdown_tx.subscribe();

    // Spawn a reaper task.
    let registry_reaper = registry.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let dead = registry_reaper.lock().await.reap_dead();
            for name in &dead {
                tracing::info!("reaped dead session: {}", name);
            }
        }
    });

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        let registry = registry.clone();
                        let shutdown = shutdown_tx.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, registry, shutdown).await;
                        });
                    }
                    Err(e) => {
                        tracing::error!("accept error: {}", e);
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("shutdown signal received");
                // Kill all sessions.
                let mut reg = registry.lock().await;
                let sessions = reg.list();
                for s in &sessions {
                    let _ = reg.kill(&s.name);
                }
                break;
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    registry: Arc<Mutex<Registry>>,
    shutdown: broadcast::Sender<()>,
) {
    let (mut reader, mut writer) = stream.into_split();

    loop {
        let msg = match try_read_frame_async::<ClientMessage>(&mut reader).await {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => {
                tracing::debug!("read error: {}", e);
                return;
            }
            None => return, // Client disconnected.
        };

        match msg {
            ClientMessage::Ping => {
                let _ = write_frame_async(&mut writer, &DaemonMessage::Pong).await;
            }
            ClientMessage::KillServer => {
                let _ = write_frame_async(&mut writer, &DaemonMessage::Ok).await;
                let _ = shutdown.send(());
                return;
            }
            ClientMessage::CreateSession { name, command, env } => {
                let mut reg = registry.lock().await;
                match reg.create(name, &command, 80, 24, env) {
                    Ok(name) => {
                        let _ = write_frame_async(
                            &mut writer,
                            &DaemonMessage::SessionCreated { name },
                        )
                        .await;
                    }
                    Err(e) => {
                        let _ = write_frame_async(
                            &mut writer,
                            &DaemonMessage::Error(e.to_string()),
                        )
                        .await;
                    }
                }
            }
            ClientMessage::ListSessions => {
                let reg = registry.lock().await;
                let list = reg.list();
                let _ =
                    write_frame_async(&mut writer, &DaemonMessage::SessionList(list)).await;
            }
            ClientMessage::GetSessionInfo { name } => {
                let reg = registry.lock().await;
                match reg.info(&name) {
                    Some(info) => {
                        let _ = write_frame_async(
                            &mut writer,
                            &DaemonMessage::SessionDetail(info),
                        )
                        .await;
                    }
                    None => {
                        let _ = write_frame_async(
                            &mut writer,
                            &DaemonMessage::Error(format!("session '{}' not found", name)),
                        )
                        .await;
                    }
                }
            }
            ClientMessage::KillSession { name } => {
                let mut reg = registry.lock().await;
                match reg.kill(&name) {
                    Ok(()) => {
                        let _ = write_frame_async(&mut writer, &DaemonMessage::Ok).await;
                    }
                    Err(e) => {
                        let _ = write_frame_async(
                            &mut writer,
                            &DaemonMessage::Error(e.to_string()),
                        )
                        .await;
                    }
                }
            }
            ClientMessage::KillAllSessions => {
                let mut reg = registry.lock().await;
                let count = reg.kill_all();
                let _ = write_frame_async(
                    &mut writer,
                    &DaemonMessage::KilledSessions { count },
                )
                .await;
            }
            ClientMessage::Attach { name, cols, rows } => {
                // Handle attach: stream output from session.
                handle_attach(&mut reader, &mut writer, registry.clone(), &name, cols, rows)
                    .await;
                return; // Attach takes over the connection.
            }
            ClientMessage::SendInput {
                name,
                data,
                newline,
            } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let mut payload = data;
                    if newline {
                        payload.push(b'\n');
                    }
                    let _ = session.input_tx.send(payload).await;
                    let _ =
                        write_frame_async(&mut writer, &DaemonMessage::InputSent).await;
                } else {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                }
            }
            ClientMessage::HasSession { name } => {
                let reg = registry.lock().await;
                let exists = reg.get(&name).is_some();
                let _ =
                    write_frame_async(&mut writer, &DaemonMessage::SessionExists(exists)).await;
            }
            ClientMessage::CaptureScrollback { name, lines } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let data = session
                        .scrollback
                        .lock()
                        .map(|sb| sb.last_lines(lines))
                        .unwrap_or_default();
                    let _ =
                        write_frame_async(&mut writer, &DaemonMessage::CaptureOutput(data)).await;
                } else {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                }
            }
            ClientMessage::SetEnv { name, key, value } => {
                let mut reg = registry.lock().await;
                if let Some(session) = reg.get_mut(&name) {
                    session.env_vars.insert(key, value);
                    let _ = write_frame_async(&mut writer, &DaemonMessage::Ok).await;
                } else {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                }
            }
            ClientMessage::GetEnv { name, key } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let value = session.env_vars.get(&key).cloned();
                    let _ =
                        write_frame_async(&mut writer, &DaemonMessage::EnvValue(value)).await;
                } else {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                }
            }
            ClientMessage::GetAllEnv { name } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let vars = session.env_vars.clone();
                    let _ =
                        write_frame_async(&mut writer, &DaemonMessage::EnvVars(vars)).await;
                } else {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                }
            }
            ClientMessage::WaitSession { name, timeout_secs } => {
                // Subscribe to session's output to detect when it closes.
                let mut output_rx = {
                    let reg = registry.lock().await;
                    match reg.get(&name) {
                        Some(session) => {
                            // If already dead, return immediately.
                            if !session.is_alive() {
                                let _ = write_frame_async(
                                    &mut writer,
                                    &DaemonMessage::SessionExited,
                                )
                                .await;
                                continue;
                            }
                            session.output_tx.subscribe()
                        }
                        None => {
                            let _ = write_frame_async(
                                &mut writer,
                                &DaemonMessage::Error(format!("session '{}' not found", name)),
                            )
                            .await;
                            continue;
                        }
                    }
                };

                // Wait for output channel to close (session ended) or timeout.
                let wait_fut = async {
                    loop {
                        match output_rx.recv().await {
                            Ok(_) => continue, // Discard output, just wait.
                            Err(broadcast::error::RecvError::Closed) => break,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        }
                    }
                };

                if timeout_secs > 0 {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(timeout_secs),
                        wait_fut,
                    )
                    .await
                    {
                        Ok(()) => {
                            let _ = write_frame_async(
                                &mut writer,
                                &DaemonMessage::SessionExited,
                            )
                            .await;
                        }
                        Err(_) => {
                            let _ = write_frame_async(
                                &mut writer,
                                &DaemonMessage::Error("timeout".to_string()),
                            )
                            .await;
                        }
                    }
                } else {
                    wait_fut.await;
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::SessionExited,
                    )
                    .await;
                }
            }
            _ => {
                let _ = write_frame_async(
                    &mut writer,
                    &DaemonMessage::Error("unexpected message".to_string()),
                )
                .await;
            }
        }
    }
}

async fn handle_attach(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    registry: Arc<Mutex<Registry>>,
    name: &str,
    cols: u16,
    rows: u16,
) {
    // Get session handles (brief lock, no scrollback mutation needed).
    let (input_tx, mut output_rx, resize_tx, scrollback_data) = {
        let reg = registry.lock().await;
        let session = match reg.get(name) {
            Some(s) => s,
            None => {
                let _ = write_frame_async(
                    writer,
                    &DaemonMessage::Error(format!("session '{}' not found", name)),
                )
                .await;
                return;
            }
        };

        let input_tx = session.input_tx.clone();
        let output_rx = session.output_tx.subscribe();
        let resize_tx = session.resize_tx.clone();
        // Read scrollback from the session's Arc (short std::sync::Mutex lock).
        let scrollback = session
            .scrollback
            .lock()
            .map(|sb| sb.contents())
            .unwrap_or_default();

        (input_tx, output_rx, resize_tx, scrollback)
    };

    // Resize to client's terminal size (outside registry lock).
    let _ = resize_tx.send((cols, rows)).await;

    // Send scrollback first.
    if !scrollback_data.is_empty() {
        let _ = write_frame_async(writer, &DaemonMessage::Output(scrollback_data)).await;
    }

    // Bidirectional streaming (no registry lock needed in the hot path).
    loop {
        tokio::select! {
            // Output from PTY → client.
            output = output_rx.recv() => {
                match output {
                    Ok(data) => {
                        // Scrollback is now stored by io_loop in session.rs,
                        // so no registry lock needed here.
                        if write_frame_async(writer, &DaemonMessage::Output(data)).await.is_err() {
                            return; // Client disconnected.
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("output lagged by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = write_frame_async(writer, &DaemonMessage::SessionEnded).await;
                        return;
                    }
                }
            }
            // Input from client → PTY.
            msg = try_read_frame_async::<ClientMessage>(reader) => {
                match msg {
                    Some(Ok(ClientMessage::AttachInput(data))) => {
                        let _ = input_tx.send(data).await;
                    }
                    Some(Ok(ClientMessage::AttachResize { cols, rows })) => {
                        let _ = resize_tx.send((cols, rows)).await;
                    }
                    Some(Ok(ClientMessage::Detach)) | None => {
                        return; // Client detached or disconnected.
                    }
                    Some(Ok(_)) => {
                        // Ignore unexpected messages during attach.
                    }
                    Some(Err(e)) => {
                        tracing::debug!("attach read error: {}", e);
                        return;
                    }
                }
            }
        }
    }
}
