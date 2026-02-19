use std::io::Write;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use futures::StreamExt;

use crate::protocol::codec::{try_read_frame_async, write_frame_async};
use crate::protocol::messages::{ClientMessage, DaemonMessage};

/// Run the attach loop: bidirectional I/O between terminal and daemon.
pub async fn run_attach(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> anyhow::Result<()> {
    // Enable raw mode.
    terminal::enable_raw_mode()?;

    let result = attach_loop(reader, writer).await;

    // Always restore terminal.
    terminal::disable_raw_mode()?;

    result
}

async fn attach_loop(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> anyhow::Result<()> {
    let mut event_stream = EventStream::new();
    let mut prefix_pending = false;
    let mut stdout = std::io::stdout();

    loop {
        tokio::select! {
            // Data from daemon → terminal stdout.
            msg = try_read_frame_async::<DaemonMessage>(reader) => {
                match msg {
                    Some(Ok(DaemonMessage::Output(data))) => {
                        stdout.write_all(&data)?;
                        stdout.flush()?;
                    }
                    Some(Ok(DaemonMessage::SessionEnded)) => {
                        eprintln!("\r\namux: session ended");
                        return Ok(());
                    }
                    Some(Ok(DaemonMessage::Error(e))) => {
                        eprintln!("\r\namux: error: {}", e);
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        eprintln!("\r\namux: connection error: {}", e);
                        return Ok(());
                    }
                    None => {
                        eprintln!("\r\namux: disconnected from server");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            // Terminal events → daemon.
            event = event_stream.next() => {
                match event {
                    Some(Ok(Event::Key(key_event))) => {
                        if let Some(action) = handle_key(key_event, &mut prefix_pending) {
                            match action {
                                KeyAction::Detach => {
                                    let _ = write_frame_async(writer, &ClientMessage::Detach).await;
                                    eprintln!("\r\namux: detached");
                                    return Ok(());
                                }
                                KeyAction::Send(data) => {
                                    let _ = write_frame_async(
                                        writer,
                                        &ClientMessage::AttachInput(data),
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        let _ = write_frame_async(
                            writer,
                            &ClientMessage::AttachResize { cols, rows },
                        )
                        .await;
                    }
                    Some(Ok(Event::Paste(text))) => {
                        let _ = write_frame_async(
                            writer,
                            &ClientMessage::AttachInput(text.into_bytes()),
                        )
                        .await;
                    }
                    Some(Err(_)) | None => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
}

enum KeyAction {
    Detach,
    Send(Vec<u8>),
}

/// Handle a key event, managing the Ctrl+B prefix key for detach.
fn handle_key(event: KeyEvent, prefix_pending: &mut bool) -> Option<KeyAction> {
    // Ctrl+B is our prefix key (like tmux).
    if event.modifiers.contains(KeyModifiers::CONTROL) && event.code == KeyCode::Char('b') {
        if !*prefix_pending {
            *prefix_pending = true;
            return None; // Eat the prefix key.
        }
        // Double Ctrl+B sends a literal Ctrl+B.
        *prefix_pending = false;
        return Some(KeyAction::Send(vec![2])); // Ctrl+B = 0x02
    }

    if *prefix_pending {
        *prefix_pending = false;
        match event.code {
            KeyCode::Char('d') | KeyCode::Char('D') => {
                return Some(KeyAction::Detach);
            }
            _ => {
                // Unknown prefix command, ignore.
                return None;
            }
        }
    }

    // Normal key → convert to bytes.
    let bytes = key_to_bytes(event)?;
    Some(KeyAction::Send(bytes))
}

fn key_to_bytes(event: KeyEvent) -> Option<Vec<u8>> {
    match event.code {
        KeyCode::Char(c) => {
            if event.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+letter → 1-26
                let ctrl = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                Some(vec![ctrl])
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                Some(s.as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![127]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![27]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}
