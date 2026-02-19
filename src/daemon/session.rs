use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command as StdCommand;

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
    pub input_tx: mpsc::Sender<Vec<u8>>,
    pub output_tx: broadcast::Sender<Vec<u8>>,
    pub resize_tx: mpsc::Sender<(u16, u16)>,
    pub kill_tx: Option<oneshot::Sender<()>>,
    pub scrollback: Scrollback,
    master_fd: i32,
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
}

impl Session {
    /// Spawn a new session with the given command.
    pub fn spawn(
        name: String,
        cmd: &[String],
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Self> {
        let winsize = Winsize {
            ws_row: if rows > 0 { rows } else { 24 },
            ws_col: if cols > 0 { cols } else { 80 },
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // Open a PTY pair.
        let pty = openpty(Some(&winsize), None)?;
        let master_fd = pty.master.as_raw_fd();
        let slave_fd = pty.slave.as_raw_fd();

        // Set slave to raw mode equivalent (disable echo/canon for agents).
        let mut termios_settings = termios::tcgetattr(&pty.slave)?;
        termios::cfmakeraw(&mut termios_settings);
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
                let err = StdCommand::new(program).args(args).exec();
                eprintln!("amux: exec failed: {}", err);
                std::process::exit(1);
            }
            ForkResult::Parent { child } => child,
        };

        // Close slave side in parent.
        drop(pty.slave);

        // Keep master_fd alive by leaking the OwnedFd (we manage it manually).
        let master_raw = pty.master.as_raw_fd();
        std::mem::forget(pty.master);

        // Create channels.
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(256);
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(256);
        let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>(16);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        let output_tx_clone = output_tx.clone();
        let command_str = cmd.join(" ");

        let mut session = Session {
            name,
            command: command_str,
            child_pid,
            input_tx,
            output_tx,
            resize_tx,
            kill_tx: Some(kill_tx),
            scrollback: Scrollback::new(),
            master_fd: master_raw,
        };

        // Spawn the I/O task.
        // We use a separate task that owns the master fd.
        tokio::spawn(Self::io_loop(
            master_raw,
            child_pid,
            input_rx,
            output_tx_clone,
            resize_rx,
            kill_rx,
        ));

        Ok(session)
    }

    async fn io_loop(
        master_fd: i32,
        child_pid: nix::unistd::Pid,
        mut input_rx: mpsc::Receiver<Vec<u8>>,
        output_tx: broadcast::Sender<Vec<u8>>,
        mut resize_rx: mpsc::Receiver<(u16, u16)>,
        mut kill_rx: oneshot::Receiver<()>,
    ) {
        // Wrap the master fd in async I/O.
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
        match nix::sys::signal::kill(self.child_pid, None) {
            Ok(_) => true,
            Err(_) => false,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Close the master fd if still open.
        unsafe {
            libc::close(self.master_fd);
        }
    }
}
