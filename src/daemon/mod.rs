pub mod registry;
pub mod server;
pub mod session;
pub mod vterm;
pub mod watchdog;

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

    // Daemon is only "running" when both the socket is accepting
    // connections AND the pid file points to a live amux process. If
    // either signal is stale (kill -9 leaves both behind; ordinary
    // shutdown leaves neither), clear everything and fork fresh.
    if common::daemon_alive() {
        bail!("server is already running");
    }
    common::clear_stale_runtime_files()
        .with_context(|| format!("failed to clear stale runtime files in {}", run_dir.display()))?;

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

            // On macOS, ask the kernel to keep us scheduled. App Nap is
            // applied per-app and primarily targets foreground apps with
            // a Dock entry, but the practical risk for amux is system
            // idle sleep (closed lid, inactive desktop) — `caffeinate -i`
            // suppresses that and exits when our pid does.
            #[cfg(target_os = "macos")]
            disable_app_nap_macos(pid);

            // Now safe to create tokio runtime.
            run_daemon(sock_path);
        }
    }
}

/// Best-effort App Nap / idle-sleep opt-out for macOS.
///
/// We shell out to `caffeinate -i -w <pid>` rather than linking
/// Foundation/IOKit directly: caffeinate has shipped with macOS since
/// 10.8, the `-w` flag binds its lifetime to ours, and a missing or
/// failing binary leaves the daemon in its previous (working) state.
/// The watchdog still catches actual suspensions, so this is a
/// belt-and-braces hint to the scheduler, not a correctness primitive.
#[cfg(target_os = "macos")]
fn disable_app_nap_macos(pid: u32) {
    use std::process::{Command, Stdio};
    let _ = Command::new("caffeinate")
        .args(["-i", "-w", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
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
