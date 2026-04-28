use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};

use crate::daemon::registry::Registry;
use crate::protocol::codec::{try_read_frame_async, write_frame_async};
use crate::protocol::messages::{CaptureMode, ClientMessage, DaemonMessage};

/// Strip CSI escape sequences (ESC `[` ... final-byte) from `bytes`. The
/// final byte of a CSI sequence is in the range 0x40..=0x7E. Used for
/// `CaptureMode::Plain` to drop SGR codes after vt100-replay rendering.
fn strip_csi_escapes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                if (0x40..=0x7E).contains(&b) {
                    break;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

pub async fn run_server(listener: UnixListener, shutdown_tx: broadcast::Sender<()>) {
    let registry = Arc::new(Mutex::new(Registry::new()));
    let mut shutdown_rx = shutdown_tx.subscribe();

    // Spawn the suspension-aware watchdog. It detects macOS App Nap /
    // system sleep via monotonic-clock gaps, reaps zombie children that
    // exited during the suspension, and runs the periodic dead-session
    // sweep that the previous reaper handled.
    let registry_watchdog = registry.clone();
    tokio::spawn(async move {
        crate::daemon::watchdog::run(registry_watchdog).await;
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
            ClientMessage::CreateSession { name, command, env, cwd, cols, rows } => {
                let mut reg = registry.lock().await;
                match reg.create(name, &command, cols.unwrap_or(80), rows.unwrap_or(24), env, cwd) {
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
                // Attach takes ownership of reader/writer (connection is consumed).
                handle_attach(reader, writer, registry.clone(), &name, cols, rows)
                    .await;
                return;
            }
            ClientMessage::Follow { name } => {
                // Follow takes ownership of the connection (read-only streaming).
                handle_follow(reader, writer, registry.clone(), &name).await;
                return;
            }
            ClientMessage::SendInput {
                name,
                data,
                newline,
            } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let _ = session.input_tx.send(data).await;
                    if newline {
                        let _ = session.input_tx.send(vec![b'\r']).await;
                    }
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
            ClientMessage::CaptureScrollback { name, lines, mode } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let data = match mode {
                        CaptureMode::Raw => session
                            .scrollback
                            .lock()
                            .map(|sb| sb.last_lines(lines))
                            .unwrap_or_default(),
                        // Plain and Formatted both replay the raw scrollback
                        // ring through a temporary vt100 parser sized to fit
                        // the requested line count. This recovers history
                        // beyond the agent's PTY rows (e.g. for amux top's
                        // preview pane in tall terminals — bd-pmk).
                        CaptureMode::Plain => {
                            let raw = session
                                .scrollback
                                .lock()
                                .map(|sb| sb.contents())
                                .unwrap_or_default();
                            let cols = session
                                .vterm
                                .lock()
                                .map(|vt| vt.size().1)
                                .unwrap_or(80);
                            let target_rows = (lines as u16).max(24);
                            let formatted = crate::daemon::vterm::render_raw_scrollback_formatted(
                                &raw, target_rows, cols, lines,
                            );
                            // Strip escape codes for Plain mode.
                            strip_csi_escapes(&formatted)
                        }
                        CaptureMode::Formatted => {
                            let raw = session
                                .scrollback
                                .lock()
                                .map(|sb| sb.contents())
                                .unwrap_or_default();
                            let cols = session
                                .vterm
                                .lock()
                                .map(|vt| vt.size().1)
                                .unwrap_or(80);
                            let target_rows = (lines as u16).max(24);
                            crate::daemon::vterm::render_raw_scrollback_formatted(
                                &raw, target_rows, cols, lines,
                            )
                        }
                    };
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
                // Subscribe to session's exit watch to detect when io_loop exits.
                let mut exit_rx = {
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
                            session.exit_watch.clone()
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

                // Wait for exit_watch to signal true (io_loop exited) or timeout.
                let wait_fut = async {
                    loop {
                        if exit_rx.changed().await.is_err() {
                            break; // Sender dropped (session cleaned up).
                        }
                        if *exit_rx.borrow() {
                            break; // io_loop signalled exit.
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
            ClientMessage::GetExitCode { name } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let code = session.exit_code.lock().ok().and_then(|ec| *ec);
                    let _ =
                        write_frame_async(&mut writer, &DaemonMessage::ExitCode(code)).await;
                } else {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                }
            }
            ClientMessage::WatchSessions { sessions } => {
                // Takes ownership of the connection (streaming).
                handle_watch(writer, registry.clone(), sessions).await;
                return;
            }
            ClientMessage::WaitAny {
                sessions,
                timeout_secs,
            } => {
                handle_wait_any(&mut writer, registry.clone(), sessions, timeout_secs).await;
            }
            ClientMessage::ResizeSession { name, cols, rows } => {
                let reg = registry.lock().await;
                if let Some(session) = reg.get(&name) {
                    let _ = session.resize_tx.send((cols, rows)).await;
                    let _ = write_frame_async(&mut writer, &DaemonMessage::Ok).await;
                } else {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
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
    mut reader: tokio::net::unix::OwnedReadHalf,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    registry: Arc<Mutex<Registry>>,
    name: &str,
    cols: u16,
    rows: u16,
) {
    // Get session handles (brief lock, no scrollback mutation needed).
    // Increment attach_count so `amux top` defers size control to us.
    let (input_tx, mut output_rx, resize_tx, mut exit_rx, scrollback_data, attach_count) = {
        let reg = registry.lock().await;
        let session = match reg.get(name) {
            Some(s) => s,
            None => {
                let _ = write_frame_async(
                    &mut writer,
                    &DaemonMessage::Error(format!("session '{}' not found", name)),
                )
                .await;
                return;
            }
        };

        let input_tx = session.input_tx.clone();
        let output_rx = session.output_tx.subscribe();
        let resize_tx = session.resize_tx.clone();
        let exit_rx = session.exit_watch.clone();
        let attach_count = session.attach_count.clone();
        // Read scrollback from the session's Arc (short std::sync::Mutex lock).
        let scrollback = session
            .scrollback
            .lock()
            .map(|sb| sb.contents())
            .unwrap_or_default();

        attach_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        (input_tx, output_rx, resize_tx, exit_rx, scrollback, attach_count)
    };

    // Resize to client's terminal size (outside registry lock).
    let _ = resize_tx.send((cols, rows)).await;

    // Send scrollback first.
    if !scrollback_data.is_empty() {
        let _ = write_frame_async(&mut writer, &DaemonMessage::Output(scrollback_data)).await;
    }

    // Spawn a dedicated reader task for client messages.
    // try_read_frame_async is NOT cancel-safe (two sequential read_exact calls),
    // so it must not be used directly in tokio::select!. A dedicated task ensures
    // the frame read is never cancelled mid-parse, preventing stream corruption.
    let (client_msg_tx, mut client_msg_rx) = tokio::sync::mpsc::channel::<ClientMessage>(32);
    let reader_task = tokio::spawn(async move {
        loop {
            match try_read_frame_async::<ClientMessage>(&mut reader).await {
                Some(Ok(msg)) => {
                    if client_msg_tx.send(msg).await.is_err() {
                        break; // Receiver dropped, attach ended.
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!("attach read error: {}", e);
                    break;
                }
                None => break, // Client disconnected.
            }
        }
    });

    // Bidirectional streaming (no registry lock needed in the hot path).
    loop {
        tokio::select! {
            // Output from PTY → client.
            output = output_rx.recv() => {
                match output {
                    Ok(data) => {
                        // Scrollback is now stored by io_loop in session.rs,
                        // so no registry lock needed here.
                        if write_frame_async(&mut writer, &DaemonMessage::Output(data)).await.is_err() {
                            break; // Client disconnected.
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("output lagged by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = write_frame_async(&mut writer, &DaemonMessage::SessionEnded).await;
                        break;
                    }
                }
            }
            // Session io_loop exited → notify client immediately.
            // The broadcast channel won't close until the Session struct is
            // dropped (by the reaper), so we watch exit_rx to avoid a 30s hang.
            _ = exit_rx.changed() => {
                if *exit_rx.borrow() {
                    let _ = write_frame_async(&mut writer, &DaemonMessage::SessionEnded).await;
                    break;
                }
            }
            // Input from client → PTY (via cancel-safe channel).
            msg = client_msg_rx.recv() => {
                match msg {
                    Some(ClientMessage::AttachInput(data)) => {
                        tracing::trace!("attach input: {} bytes", data.len());
                        let _ = input_tx.send(data).await;
                    }
                    Some(ClientMessage::AttachResize { cols, rows }) => {
                        let _ = resize_tx.send((cols, rows)).await;
                    }
                    Some(ClientMessage::Detach) | None => {
                        break; // Client detached or disconnected.
                    }
                    Some(_) => {
                        // Ignore unexpected messages during attach.
                    }
                }
            }
        }
    }

    reader_task.abort();
    // Drop attacher count so top regains size control.
    attach_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
}

async fn handle_follow(
    reader: tokio::net::unix::OwnedReadHalf,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    registry: Arc<Mutex<Registry>>,
    name: &str,
) {
    // Get session output channel and exit watch (brief lock).
    let (mut output_rx, mut exit_rx, scrollback_data) = {
        let reg = registry.lock().await;
        let session = match reg.get(name) {
            Some(s) => s,
            None => {
                let _ = write_frame_async(
                    &mut writer,
                    &DaemonMessage::Error(format!("session '{}' not found", name)),
                )
                .await;
                return;
            }
        };

        let output_rx = session.output_tx.subscribe();
        let exit_rx = session.exit_watch.clone();
        let scrollback = session
            .scrollback
            .lock()
            .map(|sb| sb.contents())
            .unwrap_or_default();

        (output_rx, exit_rx, scrollback)
    };

    // Send scrollback first.
    if !scrollback_data.is_empty() {
        if write_frame_async(&mut writer, &DaemonMessage::Output(scrollback_data))
            .await
            .is_err()
        {
            return;
        }
    }

    // Spawn a reader task to detect client disconnect (e.g. Ctrl+C / Detach).
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::channel::<()>(1);
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match try_read_frame_async::<ClientMessage>(&mut reader).await {
                Some(Ok(ClientMessage::Detach)) | None => {
                    let _ = disconnect_tx.send(()).await;
                    break;
                }
                Some(Err(_)) => {
                    let _ = disconnect_tx.send(()).await;
                    break;
                }
                Some(Ok(_)) => {
                    // Ignore unexpected messages during follow.
                }
            }
        }
    });

    // Stream output to the client (read-only, no stdin forwarding).
    loop {
        tokio::select! {
            output = output_rx.recv() => {
                match output {
                    Ok(data) => {
                        if write_frame_async(&mut writer, &DaemonMessage::Output(data)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("follow output lagged by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = write_frame_async(&mut writer, &DaemonMessage::SessionEnded).await;
                        break;
                    }
                }
            }
            _ = exit_rx.changed() => {
                if *exit_rx.borrow() {
                    let _ = write_frame_async(&mut writer, &DaemonMessage::SessionEnded).await;
                    break;
                }
            }
            _ = disconnect_rx.recv() => {
                break; // Client disconnected or sent Detach.
            }
        }
    }

    reader_task.abort();
}

async fn handle_watch(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    registry: Arc<Mutex<Registry>>,
    sessions: Vec<String>,
) {
    use std::collections::HashMap;
    use tokio::sync::watch;

    if sessions.is_empty() {
        let _ = write_frame_async(
            &mut writer,
            &DaemonMessage::Error("no sessions specified".to_string()),
        )
        .await;
        return;
    }

    // Collect exit_watch receivers and exit_code handles for each session.
    // For sessions that are already dead, send the exit event immediately.
    let mut watchers: HashMap<String, (watch::Receiver<bool>, std::sync::Arc<std::sync::Mutex<Option<i32>>>)> =
        HashMap::new();

    {
        let reg = registry.lock().await;
        for name in &sessions {
            match reg.get(name) {
                Some(session) => {
                    if !session.is_alive() {
                        // Already dead — send exit event immediately.
                        let code = session.exit_code.lock().ok().and_then(|ec| *ec);
                        if write_frame_async(
                            &mut writer,
                            &DaemonMessage::WatchSessionExited {
                                session: name.clone(),
                                exit_code: code,
                            },
                        )
                        .await
                        .is_err()
                        {
                            return; // Client disconnected.
                        }
                    } else {
                        watchers.insert(
                            name.clone(),
                            (session.exit_watch.clone(), session.exit_code.clone()),
                        );
                    }
                }
                None => {
                    let _ = write_frame_async(
                        &mut writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                    return;
                }
            }
        }
    }

    // Watch remaining live sessions using a JoinSet to avoid futures_util dependency.
    while !watchers.is_empty() {
        let exited_session = {
            let mut join_set = tokio::task::JoinSet::new();
            for (name, (rx, _)) in &watchers {
                let name = name.clone();
                let mut rx = rx.clone();
                join_set.spawn(async move {
                    loop {
                        if rx.changed().await.is_err() {
                            return name; // Sender dropped.
                        }
                        if *rx.borrow() {
                            return name; // io_loop signalled exit.
                        }
                    }
                });
            }
            // Wait for the first session to exit.
            match join_set.join_next().await {
                Some(Ok(name)) => {
                    join_set.abort_all();
                    name
                }
                _ => break, // Shouldn't happen.
            }
        };

        // Get exit code for the exited session.
        let exit_code = watchers
            .get(&exited_session)
            .and_then(|(_, ec)| ec.lock().ok().and_then(|ec| *ec));

        watchers.remove(&exited_session);

        if write_frame_async(
            &mut writer,
            &DaemonMessage::WatchSessionExited {
                session: exited_session,
                exit_code,
            },
        )
        .await
        .is_err()
        {
            return; // Client disconnected.
        }
    }

    // All sessions have exited.
    let _ = write_frame_async(&mut writer, &DaemonMessage::WatchDone).await;
}

/// Handle WaitAny: block until the first of the given sessions exits, or timeout.
async fn handle_wait_any(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    registry: std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::Registry>>,
    sessions: Vec<String>,
    timeout_secs: u64,
) {
    use std::collections::HashMap;
    use tokio::sync::watch;

    if sessions.is_empty() {
        let _ = write_frame_async(
            writer,
            &DaemonMessage::Error("no sessions specified".to_string()),
        )
        .await;
        return;
    }

    // Check sessions and collect watchers. If any session is already dead, return it immediately.
    let mut watchers: HashMap<String, (watch::Receiver<bool>, std::sync::Arc<std::sync::Mutex<Option<i32>>>)> =
        HashMap::new();

    {
        let reg = registry.lock().await;
        for name in &sessions {
            match reg.get(name) {
                Some(session) => {
                    if !session.is_alive() {
                        // Already dead — return immediately.
                        let code = session.exit_code.lock().ok().and_then(|ec| *ec);
                        let _ = write_frame_async(
                            writer,
                            &DaemonMessage::WaitAnyExited {
                                session: name.clone(),
                                exit_code: code,
                            },
                        )
                        .await;
                        return;
                    }
                    watchers.insert(
                        name.clone(),
                        (session.exit_watch.clone(), session.exit_code.clone()),
                    );
                }
                None => {
                    let _ = write_frame_async(
                        writer,
                        &DaemonMessage::Error(format!("session '{}' not found", name)),
                    )
                    .await;
                    return;
                }
            }
        }
    }

    // All sessions are alive — race them with a JoinSet.
    let wait_fut = async {
        let mut join_set = tokio::task::JoinSet::new();
        for (name, (rx, _)) in &watchers {
            let name = name.clone();
            let mut rx = rx.clone();
            join_set.spawn(async move {
                loop {
                    if rx.changed().await.is_err() {
                        return name;
                    }
                    if *rx.borrow() {
                        return name;
                    }
                }
            });
        }
        match join_set.join_next().await {
            Some(Ok(name)) => {
                join_set.abort_all();
                name
            }
            _ => String::new(), // Shouldn't happen.
        }
    };

    let result = if timeout_secs > 0 {
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), wait_fut).await {
            Ok(name) => Some(name),
            Err(_) => None,
        }
    } else {
        Some(wait_fut.await)
    };

    match result {
        Some(name) if !name.is_empty() => {
            let exit_code = watchers
                .get(&name)
                .and_then(|(_, ec)| ec.lock().ok().and_then(|ec| *ec));
            let _ = write_frame_async(
                writer,
                &DaemonMessage::WaitAnyExited {
                    session: name,
                    exit_code,
                },
            )
            .await;
        }
        None => {
            let _ = write_frame_async(
                writer,
                &DaemonMessage::Error("timeout".to_string()),
            )
            .await;
        }
        _ => {} // Empty name — shouldn't happen.
    }
}
