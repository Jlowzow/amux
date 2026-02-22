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
    reader: &mut tokio::net::unix::OwnedReadHalf,
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

async fn attach_loop(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> anyhow::Result<()> {
    let async_stdin = AsyncFd::new(NonOwningFd(libc::STDIN_FILENO))?;
    let mut sigwinch = signal(SignalKind::window_change())?;

    let mut prefix_pending = false;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 4096];

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
                    Ok(Ok(0)) => return Ok(()), // EOF
                    Ok(Ok(n)) => {
                        let data = &buf[..n];
                        tracing::trace!("stdin: {} raw bytes", n);
                        match process_raw_input(data, &mut prefix_pending) {
                            Some(InputAction::Detach) => {
                                let _ = write_frame_async(writer, &ClientMessage::Detach).await;
                                eprintln!("\r\namux: detached");
                                return Ok(());
                            }
                            Some(InputAction::Send(bytes)) => {
                                let _ = write_frame_async(
                                    writer,
                                    &ClientMessage::AttachInput(bytes),
                                ).await;
                            }
                            None => {}
                        }
                    }
                    Ok(Err(e)) => {
                        if e.kind() != std::io::ErrorKind::WouldBlock {
                            return Err(e.into());
                        }
                    }
                    Err(_would_block) => continue,
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
    }
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
