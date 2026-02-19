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
    let path = runtime_dir().join("server.pid");
    std::fs::write(path, pid.to_string())
}

/// Read the daemon PID from the pid file.
pub fn read_pid_file() -> io::Result<u32> {
    let path = runtime_dir().join("server.pid");
    let contents = std::fs::read_to_string(path)?;
    contents
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
