pub mod registry;
pub mod server;
pub mod session;

use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use anyhow::{bail, Context};
use nix::unistd::{self, ForkResult};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::common;

/// Fork a daemon process and start the server.
///
/// The parent process returns `Ok(())` after the fork.
/// The child process never returns (it runs the server loop).
pub fn fork_daemon() -> anyhow::Result<()> {
    let sock_path = common::socket_path();
    let run_dir = common::runtime_dir();

    // Create runtime directory.
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create runtime dir: {}", run_dir.display()))?;

    // Clean stale socket.
    if sock_path.exists() {
        if common::server_running() {
            bail!("server is already running");
        }
        fs::remove_file(&sock_path)
            .with_context(|| format!("failed to remove stale socket: {}", sock_path.display()))?;
    }

    // Fork: daemon must fork BEFORE tokio runtime.
    match unsafe { unistd::fork() }.context("fork failed")? {
        ForkResult::Parent { child } => {
            eprintln!("amux: server started (pid {})", child);
            Ok(())
        }
        ForkResult::Child => {
            // Become session leader (double-fork not needed for our use case).
            unistd::setsid().context("setsid failed")?;

            // Redirect stdin/stdout/stderr to /dev/null.
            let devnull = fs::File::open("/dev/null").context("failed to open /dev/null")?;
            let fd = devnull.as_raw_fd();
            let _ = unistd::dup2(fd, 0);
            let _ = unistd::dup2(fd, 1);
            let _ = unistd::dup2(fd, 2);

            // Write PID file.
            let pid = std::process::id();
            let _ = common::write_pid_file(pid);

            // Set up tracing to a log file.
            setup_tracing(&run_dir);

            // Now safe to create tokio runtime.
            run_daemon(sock_path);
        }
    }
}

fn setup_tracing(run_dir: &std::path::Path) {
    let log_path = run_dir.join("daemon.log");
    if let Ok(file) = fs::File::create(&log_path) {
        use tracing_subscriber::fmt;
        use tracing_subscriber::EnvFilter;

        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info"));

        let _ = fmt::Subscriber::builder()
            .with_env_filter(filter)
            .with_writer(move || file.try_clone().unwrap())
            .with_ansi(false)
            .try_init();
    }
}

fn run_daemon(sock_path: PathBuf) -> ! {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async move {
        let listener = UnixListener::bind(&sock_path).expect("failed to bind socket");

        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        // Set up signal handling.
        let shutdown_signal = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            )
            .expect("failed to register SIGTERM handler");
            let mut sighup = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::hangup(),
            )
            .expect("failed to register SIGHUP handler");

            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM");
                }
                _ = sighup.recv() => {
                    tracing::info!("received SIGHUP");
                }
            }
            let _ = shutdown_signal.send(());
        });

        server::run_server(listener, shutdown_tx).await;

        // Clean up.
        let _ = fs::remove_file(&sock_path);
        let _ = fs::remove_file(common::runtime_dir().join("server.pid"));
        tracing::info!("daemon shutdown complete");
    });

    std::process::exit(0);
}
