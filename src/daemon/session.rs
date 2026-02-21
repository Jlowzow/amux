use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex as StdMutex};

use nix::libc;
use nix::pty::{openpty, Winsize};
use nix::sys::termios;
use nix::unistd::{self, ForkResult};
use tokio::sync::{broadcast, mpsc, oneshot};

const SCROLLBACK_SIZE: usize = 64 * 1024; // 64KB

pub struct Session {
    pub name: String,
    pub command: String,
    pub child_pid: nix::unistd::Pid,
    pub created_at: std::time::SystemTime,
    /// Timestamp of last PTY output (updated by io_loop).
    pub last_activity: Arc<StdMutex<std::time::SystemTime>>,
    pub input_tx: mpsc::Sender<Vec<u8>>,
    pub output_tx: broadcast::Sender<Vec<u8>>,
    pub resize_tx: mpsc::Sender<(u16, u16)>,
    pub kill_tx: Option<oneshot::Sender<()>>,
    pub scrollback: Arc<StdMutex<Scrollback>>,
    /// Session-level metadata environment variables (not process env).
    pub env_vars: HashMap<String, String>,
}

pub struct Scrollback {
    buf: VecDeque<u8>,
}

impl Scrollback {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(SCROLLBACK_SIZE),
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        for &b in data {
            if self.buf.len() >= SCROLLBACK_SIZE {
                self.buf.pop_front();
            }
            self.buf.push_back(b);
        }
    }

    pub fn contents(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    /// Return the last `n` lines from the scrollback buffer.
    /// Lines are delimited by `\n`. If fewer than `n` lines exist,
    /// returns the entire buffer contents.
    pub fn last_lines(&self, n: usize) -> Vec<u8> {
        if n == 0 || self.buf.is_empty() {
            return Vec::new();
        }

        // Walk backwards counting newlines.
        // We want n lines, which means we need to find the (n)th '\n' from the end
        // (skipping a trailing newline if present).
        let len = self.buf.len();
        let mut newline_count = 0;
        let mut start = 0;

        // If buffer ends with '\n', skip it so we don't count an empty trailing line.
        let search_end = if self.buf[len - 1] == b'\n' {
            len - 1
        } else {
            len
        };

        for i in (0..search_end).rev() {
            if self.buf[i] == b'\n' {
                newline_count += 1;
                if newline_count == n {
                    start = i + 1;
                    break;
                }
            }
        }

        self.buf.range(start..).copied().collect()
    }
}

impl Session {
    /// Spawn a new session with the given command.
    pub fn spawn(
        name: String,
        cmd: &[String],
        cols: u16,
        rows: u16,
        env: Option<std::collections::HashMap<String, String>>,
    ) -> anyhow::Result<Self> {
        let winsize = Winsize {
            ws_row: if rows > 0 { rows } else { 24 },
            ws_col: if cols > 0 { cols } else { 80 },
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // Open a PTY pair.
        let pty = openpty(Some(&winsize), None)?;
        let slave_fd = pty.slave.as_raw_fd();

        // Start from raw mode (disables canonical processing, signals, etc.)
        // but re-enable ECHO so that input written to the master side is
        // echoed back through the slave's output — matching normal terminal
        // behaviour where typed commands are visible.
        let mut termios_settings = termios::tcgetattr(&pty.slave)?;
        termios::cfmakeraw(&mut termios_settings);
        termios_settings.local_flags.insert(
            termios::LocalFlags::ECHO
                | termios::LocalFlags::ECHOE
                | termios::LocalFlags::ECHOK
                | termios::LocalFlags::ECHOCTL,
        );
        termios::tcsetattr(&pty.slave, termios::SetArg::TCSANOW, &termios_settings)?;

        // Fork child process.
        let child_pid = match unsafe { unistd::fork() }? {
            ForkResult::Child => {
                // Close master side in child.
                drop(pty.master);

                // Create new session and set controlling terminal.
                unistd::setsid().ok();
                unsafe {
                    // TIOCSCTTY - set controlling terminal
                    libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                }

                // Dup slave to stdin/stdout/stderr.
                unistd::dup2(slave_fd, 0).ok();
                unistd::dup2(slave_fd, 1).ok();
                unistd::dup2(slave_fd, 2).ok();
                if slave_fd > 2 {
                    drop(pty.slave);
                }

                // Exec the command.
                let program = &cmd[0];
                let args = &cmd[1..];
                let mut command = StdCommand::new(program);
                command.args(args);
                if let Some(ref env_vars) = env {
                    command.envs(env_vars);
                }
                let err = command.exec();
                eprintln!("amux: exec failed: {}", err);
                std::process::exit(1);
            }
            ForkResult::Parent { child } => child,
        };

        // Close slave side in parent.
        drop(pty.slave);

        // Keep master_fd alive by leaking the OwnedFd (io_loop takes ownership).
        let master_raw = pty.master.as_raw_fd();
        std::mem::forget(pty.master);

        // Set non-blocking mode on master fd (required for AsyncFd).
        unsafe {
            let flags = libc::fcntl(master_raw, libc::F_GETFL);
            libc::fcntl(master_raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // Create channels.
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(256);
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(256);
        let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>(16);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        let output_tx_clone = output_tx.clone();
        let command_str = cmd.join(" ");
        let scrollback = Arc::new(StdMutex::new(Scrollback::new()));
        let scrollback_clone = scrollback.clone();
        let now = std::time::SystemTime::now();
        let last_activity = Arc::new(StdMutex::new(now));
        let last_activity_clone = last_activity.clone();

        // Spawn the I/O task (owns the master fd via OwnedFd).
        tokio::spawn(Self::io_loop(
            master_raw,
            child_pid,
            input_rx,
            output_tx_clone,
            scrollback_clone,
            last_activity_clone,
            resize_rx,
            kill_rx,
        ));

        let session = Session {
            name,
            command: command_str,
            child_pid,
            created_at: now,
            last_activity,
            input_tx,
            output_tx,
            resize_tx,
            kill_tx: Some(kill_tx),
            scrollback,
            env_vars: env.unwrap_or_default(),
        };

        Ok(session)
    }

    async fn io_loop(
        master_fd: i32,
        child_pid: nix::unistd::Pid,
        mut input_rx: mpsc::Receiver<Vec<u8>>,
        output_tx: broadcast::Sender<Vec<u8>>,
        scrollback: Arc<StdMutex<Scrollback>>,
        last_activity: Arc<StdMutex<std::time::SystemTime>>,
        mut resize_rx: mpsc::Receiver<(u16, u16)>,
        mut kill_rx: oneshot::Receiver<()>,
    ) {
        // Wrap the master fd in async I/O (fd must already be non-blocking).
        let master_file = unsafe { OwnedFd::from_raw_fd(master_fd) };
        let master_async = match tokio::io::unix::AsyncFd::new(master_file) {
            Ok(fd) => fd,
            Err(e) => {
                tracing::error!("failed to create async fd: {}", e);
                return;
            }
        };

        let mut read_buf = [0u8; 4096];

        loop {
            tokio::select! {
                // Read from PTY master → broadcast to clients.
                readable = master_async.readable() => {
                    match readable {
                        Ok(mut guard) => {
                            match guard.try_io(|fd| {
                                let raw = fd.as_raw_fd();
                                let n = unsafe {
                                    libc::read(
                                        raw,
                                        read_buf.as_mut_ptr() as *mut libc::c_void,
                                        read_buf.len(),
                                    )
                                };
                                if n < 0 {
                                    Err(std::io::Error::last_os_error())
                                } else {
                                    Ok(n as usize)
                                }
                            }) {
                                Ok(Ok(0)) => break, // EOF
                                Ok(Ok(n)) => {
                                    let data = read_buf[..n].to_vec();
                                    // Store in scrollback and update activity timestamp.
                                    if let Ok(mut sb) = scrollback.lock() {
                                        sb.push(&data);
                                    }
                                    if let Ok(mut ts) = last_activity.lock() {
                                        *ts = std::time::SystemTime::now();
                                    }
                                    let _ = output_tx.send(data);
                                }
                                Ok(Err(e)) => {
                                    if e.kind() != std::io::ErrorKind::WouldBlock {
                                        tracing::debug!("pty read error: {}", e);
                                        break;
                                    }
                                }
                                Err(_would_block) => continue,
                            }
                        }
                        Err(e) => {
                            tracing::debug!("readable error: {}", e);
                            break;
                        }
                    }
                }
                // Write client input → PTY master.
                Some(data) = input_rx.recv() => {
                    let raw = master_async.as_raw_fd();
                    let mut offset = 0;
                    while offset < data.len() {
                        let n = unsafe {
                            libc::write(
                                raw,
                                data[offset..].as_ptr() as *const libc::c_void,
                                data.len() - offset,
                            )
                        };
                        if n <= 0 {
                            break;
                        }
                        offset += n as usize;
                    }
                }
                // Handle resize.
                Some((cols, rows)) = resize_rx.recv() => {
                    let winsize = Winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        libc::ioctl(
                            master_async.as_raw_fd(),
                            libc::TIOCSWINSZ as _,
                            &winsize as *const Winsize,
                        );
                    }
                }
                // Kill signal.
                _ = &mut kill_rx => {
                    let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGTERM);
                    // Give child 2 seconds then SIGKILL.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGKILL);
                    break;
                }
            }
        }

        // Wait for child to exit.
        let _ = nix::sys::wait::waitpid(child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
    }

    pub fn is_alive(&self) -> bool {
        nix::sys::signal::kill(self.child_pid, None).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrollback_basic() {
        let mut sb = Scrollback::new();
        sb.push(b"hello world");
        assert_eq!(sb.contents(), b"hello world");
    }

    #[test]
    fn test_scrollback_empty() {
        let sb = Scrollback::new();
        assert!(sb.contents().is_empty());
    }

    #[test]
    fn test_scrollback_multiple_pushes() {
        let mut sb = Scrollback::new();
        sb.push(b"hello ");
        sb.push(b"world");
        assert_eq!(sb.contents(), b"hello world");
    }

    #[test]
    fn test_scrollback_overflow() {
        let mut sb = Scrollback::new();
        // Fill to capacity
        let data = vec![b'A'; SCROLLBACK_SIZE];
        sb.push(&data);
        assert_eq!(sb.contents().len(), SCROLLBACK_SIZE);

        // Push more data, oldest should be evicted
        sb.push(b"XYZ");
        let contents = sb.contents();
        assert_eq!(contents.len(), SCROLLBACK_SIZE);
        // Last 3 bytes should be XYZ
        assert_eq!(&contents[SCROLLBACK_SIZE - 3..], b"XYZ");
        // First bytes should be A (shifted)
        assert_eq!(contents[0], b'A');
    }

    #[test]
    fn test_scrollback_exactly_at_capacity() {
        let mut sb = Scrollback::new();
        let data = vec![b'B'; SCROLLBACK_SIZE];
        sb.push(&data);
        assert_eq!(sb.contents().len(), SCROLLBACK_SIZE);
        assert!(sb.contents().iter().all(|&b| b == b'B'));
    }

    #[test]
    fn test_last_lines_basic() {
        let mut sb = Scrollback::new();
        sb.push(b"line1\nline2\nline3\n");
        assert_eq!(sb.last_lines(2), b"line2\nline3\n");
    }

    #[test]
    fn test_last_lines_no_trailing_newline() {
        let mut sb = Scrollback::new();
        sb.push(b"line1\nline2\nline3");
        assert_eq!(sb.last_lines(2), b"line2\nline3");
    }

    #[test]
    fn test_last_lines_more_than_available() {
        let mut sb = Scrollback::new();
        sb.push(b"line1\nline2\n");
        // Asking for more lines than exist returns everything.
        assert_eq!(sb.last_lines(10), b"line1\nline2\n");
    }

    #[test]
    fn test_last_lines_zero() {
        let mut sb = Scrollback::new();
        sb.push(b"line1\nline2\n");
        assert!(sb.last_lines(0).is_empty());
    }

    #[test]
    fn test_last_lines_empty_buffer() {
        let sb = Scrollback::new();
        assert!(sb.last_lines(5).is_empty());
    }

    #[test]
    fn test_last_lines_single_line() {
        let mut sb = Scrollback::new();
        sb.push(b"only line\n");
        assert_eq!(sb.last_lines(1), b"only line\n");
    }

    #[test]
    fn test_last_lines_all() {
        let mut sb = Scrollback::new();
        sb.push(b"a\nb\nc\n");
        assert_eq!(sb.last_lines(3), b"a\nb\nc\n");
    }

    #[test]
    fn test_env_vars_set_get() {
        let mut env = HashMap::new();
        env.insert("GT_HOOK_STATUS".to_string(), "active".to_string());
        assert_eq!(env.get("GT_HOOK_STATUS"), Some(&"active".to_string()));
        assert_eq!(env.get("NONEXISTENT"), None);
    }

    #[test]
    fn test_env_vars_overwrite() {
        let mut env = HashMap::new();
        env.insert("KEY".to_string(), "val1".to_string());
        env.insert("KEY".to_string(), "val2".to_string());
        assert_eq!(env.get("KEY"), Some(&"val2".to_string()));
    }

    #[test]
    fn test_env_vars_list_all() {
        let mut env = HashMap::new();
        env.insert("A".to_string(), "1".to_string());
        env.insert("B".to_string(), "2".to_string());
        let clone = env.clone();
        assert_eq!(clone.len(), 2);
        assert_eq!(clone.get("A"), Some(&"1".to_string()));
        assert_eq!(clone.get("B"), Some(&"2".to_string()));
    }
}
