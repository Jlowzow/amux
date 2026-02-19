use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Requests from client to server.
#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Ping,
    KillServer,
}

/// Responses from server to client.
#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Pong,
    Ok,
    Error(String),
}

/// Return the directory used for amux runtime files.
pub fn runtime_dir() -> PathBuf {
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/amux-{}", uid))
}

/// Return the path to the server socket.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("server.sock")
}

/// Write a length-prefixed bincode frame to a writer.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> anyhow::Result<()> {
    let data = bincode::serialize(msg)?;
    let len = (data.len() as u32).to_be_bytes();
    w.write_all(&len)?;
    w.write_all(&data)?;
    w.flush()?;
    Ok(())
}

/// Read a length-prefixed bincode frame from a reader.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        anyhow::bail!("frame too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let msg = bincode::deserialize(&buf)?;
    Ok(msg)
}

/// Async: write a length-prefixed bincode frame.
pub async fn write_frame_async<T: Serialize>(
    w: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    msg: &T,
) -> anyhow::Result<()> {
    let data = bincode::serialize(msg)?;
    let len = (data.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&data).await?;
    w.flush().await?;
    Ok(())
}

/// Async: read a length-prefixed bincode frame.
pub async fn read_frame_async<T: for<'de> Deserialize<'de>>(
    r: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        anyhow::bail!("frame too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let msg = bincode::deserialize(&buf)?;
    Ok(msg)
}

/// Check if a server is already running by attempting to connect.
pub fn server_running() -> bool {
    let path = socket_path();
    if !path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

/// Write the daemon PID to a file for later reference.
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
