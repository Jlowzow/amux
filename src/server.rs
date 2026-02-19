use std::fs;
use std::os::unix::io::AsRawFd;

use anyhow::{bail, Context};
use nix::unistd::{self, ForkResult};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::common::{
    read_frame_async, runtime_dir, socket_path, write_frame_async, write_pid_file, Request,
    Response,
};

/// Fork a daemon process and start the server.
///
/// The parent process returns `Ok(())` after the fork.
/// The child process never returns (it runs the server loop).
pub fn fork_daemon() -> anyhow::Result<()> {
    let sock_path = socket_path();
    let run_dir = runtime_dir();

    // Create runtime directory.
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create runtime dir: {}", run_dir.display()))?;

    // Clean stale socket.
    if sock_path.exists() {
        if crate::common::server_running() {
            bail!("server is already running");
        }
        fs::remove_file(&sock_path)
            .with_context(|| format!("failed to remove stale socket: {}", sock_path.display()))?;
    }

    // Fork.
    match unsafe { unistd::fork() }.context("fork failed")? {
        ForkResult::Parent { child } => {
            eprintln!("amux: server started (pid {})", child);
            Ok(())
        }
        ForkResult::Child => {
            // Become session leader.
            unistd::setsid().context("setsid failed")?;

            // Redirect stdin/stdout/stderr to /dev/null.
            let devnull = fs::File::open("/dev/null").context("failed to open /dev/null")?;
            let fd = devnull.as_raw_fd();
            let _ = unistd::dup2(fd, 0);
            let _ = unistd::dup2(fd, 1);
            let _ = unistd::dup2(fd, 2);

            // Write PID file.
            let pid = std::process::id();
            let _ = write_pid_file(pid);

            // Run the async server (this never returns on success).
            run_server(sock_path);
        }
    }
}

/// Run the server event loop. This function does not return.
fn run_server(sock_path: std::path::PathBuf) -> ! {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async move {
        let listener = UnixListener::bind(&sock_path).expect("failed to bind socket");

        // Channel to signal shutdown.
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let shutdown = shutdown_tx.clone();
                            tokio::spawn(async move {
                                handle_connection(stream, shutdown).await;
                            });
                        }
                        Err(_) => continue,
                    }
                }
                _ = shutdown_rx.recv() => {
                    // Shutdown requested by a handler.
                    break;
                }
            }
        }

        // Clean up socket and pid file.
        let _ = fs::remove_file(&sock_path);
        let _ = fs::remove_file(runtime_dir().join("server.pid"));
    });

    std::process::exit(0);
}

async fn handle_connection(stream: tokio::net::UnixStream, shutdown: broadcast::Sender<()>) {
    let (mut reader, mut writer) = stream.into_split();

    loop {
        let req: Request = match read_frame_async(&mut reader).await {
            Ok(r) => r,
            Err(_) => return, // Client disconnected or bad frame.
        };

        match req {
            Request::Ping => {
                let _ = write_frame_async(&mut writer, &Response::Pong).await;
            }
            Request::KillServer => {
                let _ = write_frame_async(&mut writer, &Response::Ok).await;
                // Signal the main loop to shut down.
                let _ = shutdown.send(());
                return;
            }
        }
    }
}
