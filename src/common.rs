use std::io;
use std::path::PathBuf;

/// Return the directory used for amux runtime files.
pub fn runtime_dir() -> PathBuf {
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/amux-{}", uid))
}

/// Return the path to the server socket.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("server.sock")
}

/// Return the path to the server pid file.
pub fn pid_file_path() -> PathBuf {
    runtime_dir().join("server.pid")
}

/// Check if a server is already running by attempting to connect.
pub fn server_running() -> bool {
    let path = socket_path();
    if !path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

/// Write the daemon PID to a file.
pub fn write_pid_file(pid: u32) -> io::Result<()> {
    std::fs::write(pid_file_path(), pid.to_string())
}

/// Read the daemon PID from the pid file.
pub fn read_pid_file() -> io::Result<u32> {
    let contents = std::fs::read_to_string(pid_file_path())?;
    contents
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Remove the pid file if present. Not-found is treated as success.
pub fn remove_pid_file() -> io::Result<()> {
    match std::fs::remove_file(pid_file_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Check if a process with the given pid exists.
///
/// Uses `kill(pid, 0)`: `Ok` means the process exists and we can signal
/// it, `EPERM` means it exists but is owned by another user, anything
/// else (typically `ESRCH`) means it's gone.
pub fn pid_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    if pid == 0 {
        return false;
    }
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        _ => false,
    }
}

/// Return the command-name (`comm`) reported by `ps` for the given pid,
/// or `None` if the pid is dead or `ps` fails.
///
/// On macOS and Linux this is the executable basename — e.g. "amux" for
/// our daemon — and is set by `execve`, so a daemon forked from a client
/// (without execve) still reports the parent's comm.
pub fn pid_command(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let comm = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if comm.is_empty() {
        None
    } else {
        Some(comm)
    }
}

/// Check that the given pid is alive and its binary name starts with
/// "amux" (i.e. is plausibly our daemon, not a stale client pid or a
/// recycled pid belonging to some other process).
pub fn pid_is_amux(pid: u32) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let Some(cmd) = pid_command(pid) else {
        return false;
    };
    let basename = std::path::Path::new(&cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&cmd);
    basename.starts_with("amux")
}

/// Return `true` iff the pid file exists and points to a live amux
/// process. Used to detect stale/reclaimable pid files.
pub fn pid_file_points_to_amux() -> bool {
    match read_pid_file() {
        Ok(pid) => pid_is_amux(pid),
        Err(_) => false,
    }
}

/// True iff the daemon appears to be running — both the socket accepts
/// connections AND the pid file references a live amux process. Either
/// signal on its own can be stale (socket kept by a dead daemon that
/// didn't clean up, pid file left behind after SIGKILL), so we require
/// both.
pub fn daemon_alive() -> bool {
    server_running() && pid_file_points_to_amux()
}

/// Remove stale socket and pid files. Safe to call when the files are
/// absent. Callers must ensure the daemon is not running before calling.
pub fn clear_stale_runtime_files() -> io::Result<()> {
    let sock = socket_path();
    if sock.exists() {
        match std::fs::remove_file(&sock) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    remove_pid_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_reports_current_process() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_rejects_zero() {
        assert!(!pid_alive(0));
    }

    #[test]
    fn pid_alive_reports_dead_pid() {
        // Pick a pid that is extraordinarily unlikely to exist.
        assert!(!pid_alive(0x7fff_fffe));
    }

    #[test]
    fn pid_command_returns_something_for_current_process() {
        let comm = pid_command(std::process::id());
        assert!(comm.is_some(), "expected ps to return a comm string");
    }

    #[test]
    fn pid_command_none_for_dead_pid() {
        assert!(pid_command(0x7fff_fffe).is_none());
    }

    #[test]
    fn pid_is_amux_rejects_dead_pid() {
        assert!(!pid_is_amux(0x7fff_fffe));
    }

    #[test]
    fn pid_is_amux_accepts_current_test_binary() {
        // `cargo test` runs a binary named `amux-<hash>` under
        // target/debug/deps, so its comm starts with "amux".
        let pid = std::process::id();
        let comm = pid_command(pid).unwrap_or_default();
        if comm.starts_with("amux") {
            assert!(pid_is_amux(pid));
        }
    }

    #[test]
    fn remove_pid_file_is_idempotent_when_absent() {
        // Write then remove twice to confirm the second call succeeds.
        let path = pid_file_path();
        let dir = runtime_dir();
        let _ = std::fs::create_dir_all(&dir);
        let prior = std::fs::read_to_string(&path).ok();
        let _ = std::fs::remove_file(&path);
        assert!(remove_pid_file().is_ok(), "remove on missing must succeed");
        assert!(remove_pid_file().is_ok(), "second remove must succeed");
        // Restore previous contents if any to avoid breaking a concurrent daemon.
        if let Some(p) = prior {
            let _ = std::fs::write(&path, p);
        }
    }
}
