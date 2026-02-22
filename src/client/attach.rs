use std::io::Write;
use std::os::unix::io::{AsRawFd, RawFd};

use crossterm::terminal;
use nix::libc;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{signal, SignalKind};

use crate::protocol::codec::{try_read_frame_async, write_frame_async};
use crate::protocol::messages::{ClientMessage, DaemonMessage};

const CTRL_B: u8 = 0x02;

/// Non-owning wrapper around a raw fd for use with AsyncFd.
/// Does NOT close the fd on drop (stdin lifetime is managed by the process).
struct NonOwningFd(RawFd);

impl AsRawFd for NonOwningFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Run the attach loop: bidirectional I/O between terminal and daemon.
pub async fn run_attach(
    reader: tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> anyhow::Result<()> {
    // Enable raw mode.
    terminal::enable_raw_mode()?;

    // Set stdin to non-blocking for async reads.
    let stdin_fd = libc::STDIN_FILENO;
    let old_flags = nix::fcntl::fcntl(stdin_fd, nix::fcntl::FcntlArg::F_GETFL)
        .map_err(|e| anyhow::anyhow!("fcntl F_GETFL: {}", e))?;
    let mut new_flags = nix::fcntl::OFlag::from_bits_truncate(old_flags);
    new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
    nix::fcntl::fcntl(stdin_fd, nix::fcntl::FcntlArg::F_SETFL(new_flags))
        .map_err(|e| anyhow::anyhow!("fcntl F_SETFL: {}", e))?;

    let result = attach_loop(reader, writer).await;

    // Restore stdin to blocking mode and terminal to cooked mode.
    let restore_flags = nix::fcntl::OFlag::from_bits_truncate(old_flags);
    let _ = nix::fcntl::fcntl(stdin_fd, nix::fcntl::FcntlArg::F_SETFL(restore_flags));
    terminal::disable_raw_mode()?;

    result
}

/// Messages from the daemon reader task to the attach loop.
enum DaemonEvent {
    /// PTY output data to display.
    Output(Vec<u8>),
    /// Session ended normally.
    SessionEnded,
    /// Error from daemon.
    Error(String),
    /// Connection error or disconnect.
    Disconnected(String),
}

async fn attach_loop(
    mut reader: tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> anyhow::Result<()> {
    let async_stdin = AsyncFd::new(NonOwningFd(libc::STDIN_FILENO))?;
    let mut sigwinch = signal(SignalKind::window_change())?;

    // Spawn a dedicated reader task for daemon messages.
    // try_read_frame_async is NOT cancel-safe (two sequential read_exact calls),
    // so it must not be used directly in tokio::select!. A dedicated task ensures
    // the frame read is never cancelled mid-parse, preventing stream corruption.
    let (daemon_msg_tx, mut daemon_msg_rx) = tokio::sync::mpsc::channel::<DaemonEvent>(32);
    let reader_task = tokio::spawn(async move {
        loop {
            match try_read_frame_async::<DaemonMessage>(&mut reader).await {
                Some(Ok(DaemonMessage::Output(data))) => {
                    if daemon_msg_tx.send(DaemonEvent::Output(data)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(DaemonMessage::SessionEnded)) => {
                    let _ = daemon_msg_tx.send(DaemonEvent::SessionEnded).await;
                    break;
                }
                Some(Ok(DaemonMessage::Error(e))) => {
                    let _ = daemon_msg_tx.send(DaemonEvent::Error(e)).await;
                    break;
                }
                Some(Err(e)) => {
                    let _ = daemon_msg_tx
                        .send(DaemonEvent::Disconnected(format!("connection error: {}", e)))
                        .await;
                    break;
                }
                None => {
                    let _ = daemon_msg_tx
                        .send(DaemonEvent::Disconnected("disconnected from server".into()))
                        .await;
                    break;
                }
                _ => {}
            }
        }
    });

    let mut prefix_pending = false;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 4096];

    let result = loop {
        tokio::select! {
            // Data from daemon → terminal stdout (via cancel-safe channel).
            msg = daemon_msg_rx.recv() => {
                match msg {
                    Some(DaemonEvent::Output(data)) => {
                        stdout.write_all(&data)?;
                        stdout.flush()?;
                    }
                    Some(DaemonEvent::SessionEnded) => {
                        eprintln!("\r\namux: session ended");
                        break Ok(());
                    }
                    Some(DaemonEvent::Error(e)) => {
                        eprintln!("\r\namux: error: {}", e);
                        break Ok(());
                    }
                    Some(DaemonEvent::Disconnected(msg)) => {
                        eprintln!("\r\namux: {}", msg);
                        break Ok(());
                    }
                    None => {
                        eprintln!("\r\namux: disconnected from server");
                        break Ok(());
                    }
                }
            }
            // Raw stdin → daemon.
            readable = async_stdin.readable() => {
                let mut guard = readable?;
                match guard.try_io(|fd| {
                    let raw = fd.as_raw_fd();
                    let n = unsafe {
                        libc::read(
                            raw,
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(0)) => break Ok(()), // EOF
                    Ok(Ok(n)) => {
                        let data = &buf[..n];
                        if std::env::var("AMUX_DEBUG").is_ok() {
                            eprintln!("\r\namux-debug: stdin read {} bytes: {:?}", n, &data[..n.min(32)]);
                        }
                        match process_raw_input(data, &mut prefix_pending) {
                            Some(InputAction::Detach) => {
                                let _ = write_frame_async(writer, &ClientMessage::Detach).await;
                                eprintln!("\r\namux: detached");
                                break Ok(());
                            }
                            Some(InputAction::Send(ref bytes)) => {
                                if std::env::var("AMUX_DEBUG").is_ok() {
                                    eprintln!("\r\namux-debug: sending {} bytes as AttachInput", bytes.len());
                                }
                                let _ = write_frame_async(
                                    writer,
                                    &ClientMessage::AttachInput(bytes.clone()),
                                ).await;
                            }
                            None => {}
                        }
                    }
                    Ok(Err(e)) => {
                        if e.kind() != std::io::ErrorKind::WouldBlock {
                            if std::env::var("AMUX_DEBUG").is_ok() {
                                eprintln!("\r\namux-debug: stdin read error: {}", e);
                            }
                            break Err(e.into());
                        }
                    }
                    Err(_would_block) => {
                        // Spurious readiness — retry
                        continue;
                    }
                }
            }
            // SIGWINCH → resize.
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = terminal::size() {
                    let _ = write_frame_async(
                        writer,
                        &ClientMessage::AttachResize { cols, rows },
                    ).await;
                }
            }
        }
    };

    reader_task.abort();
    result
}

enum InputAction {
    Detach,
    Send(Vec<u8>),
}

/// Process raw input bytes. Only intercepts Ctrl+B (0x02) for the detach prefix.
/// All other bytes are forwarded verbatim to the session.
fn process_raw_input(data: &[u8], prefix_pending: &mut bool) -> Option<InputAction> {
    // Fast path: no prefix pending and no Ctrl+B in data → forward verbatim.
    if !*prefix_pending && !data.contains(&CTRL_B) {
        return Some(InputAction::Send(data.to_vec()));
    }

    let mut output = Vec::with_capacity(data.len());

    for &byte in data {
        if *prefix_pending {
            *prefix_pending = false;
            match byte {
                b'd' | b'D' => return Some(InputAction::Detach),
                CTRL_B => output.push(CTRL_B), // Double Ctrl+B → literal Ctrl+B.
                _ => {} // Unknown prefix command → discard.
            }
        } else if byte == CTRL_B {
            *prefix_pending = true;
        } else {
            output.push(byte);
        }
    }

    if output.is_empty() {
        None
    } else {
        Some(InputAction::Send(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_input_passthrough() {
        let mut prefix = false;
        let result = process_raw_input(b"hello", &mut prefix);
        assert!(matches!(result, Some(InputAction::Send(ref d)) if d == b"hello"));
        assert!(!prefix);
    }

    #[test]
    fn test_raw_input_ctrl_b_sets_prefix() {
        let mut prefix = false;
        let result = process_raw_input(&[CTRL_B], &mut prefix);
        assert!(result.is_none());
        assert!(prefix);
    }

    #[test]
    fn test_raw_input_ctrl_b_d_detaches() {
        let mut prefix = true;
        let result = process_raw_input(b"d", &mut prefix);
        assert!(matches!(result, Some(InputAction::Detach)));
    }

    #[test]
    fn test_raw_input_ctrl_b_upper_d_detaches() {
        let mut prefix = true;
        let result = process_raw_input(b"D", &mut prefix);
        assert!(matches!(result, Some(InputAction::Detach)));
    }

    #[test]
    fn test_raw_input_double_ctrl_b_sends_literal() {
        let mut prefix = true;
        let result = process_raw_input(&[CTRL_B], &mut prefix);
        assert!(matches!(result, Some(InputAction::Send(ref d)) if d == &[CTRL_B]));
        assert!(!prefix);
    }

    #[test]
    fn test_raw_input_unknown_prefix_discards() {
        let mut prefix = true;
        let result = process_raw_input(b"x", &mut prefix);
        assert!(result.is_none());
        assert!(!prefix);
    }

    #[test]
    fn test_raw_input_ctrl_b_in_middle_of_data() {
        let mut prefix = false;
        // "ab" + Ctrl+B + "cd" → sends "abcd" with prefix pending after Ctrl+B?
        // No: "ab" then Ctrl+B sets prefix, then 'c' resolves prefix (unknown → discard),
        // then 'd' is normal.
        let data = [b'a', b'b', CTRL_B, b'c', b'd'];
        let result = process_raw_input(&data, &mut prefix);
        // 'a','b' → output, Ctrl+B → prefix, 'c' → unknown prefix discard, 'd' → output
        assert!(matches!(result, Some(InputAction::Send(ref d)) if d == b"abd"));
        assert!(!prefix);
    }

    #[test]
    fn test_raw_input_escape_sequences_passthrough() {
        let mut prefix = false;
        // Arrow up: ESC [ A
        let data = b"\x1b[A";
        let result = process_raw_input(data, &mut prefix);
        assert!(matches!(result, Some(InputAction::Send(ref d)) if d == b"\x1b[A"));
    }

    #[test]
    fn test_raw_input_ctrl_b_at_end_leaves_prefix() {
        let mut prefix = false;
        let data = [b'x', CTRL_B];
        let result = process_raw_input(&data, &mut prefix);
        assert!(matches!(result, Some(InputAction::Send(ref d)) if d == b"x"));
        assert!(prefix); // Ctrl+B at end leaves prefix pending for next read.
    }
}
