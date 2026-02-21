pub mod attach;

use std::os::unix::net::UnixStream;

use anyhow::Context;

use crate::common;
use crate::protocol::codec::{read_frame, write_frame};
use crate::protocol::messages::{ClientMessage, DaemonMessage};

/// Connect to the daemon and return the stream.
pub fn connect() -> anyhow::Result<UnixStream> {
    let path = common::socket_path();
    UnixStream::connect(&path)
        .with_context(|| format!("failed to connect to server at {}", path.display()))
}

/// Send a request and read the response (sync, for simple commands).
pub fn request(req: &ClientMessage) -> anyhow::Result<DaemonMessage> {
    let mut stream = connect().context("failed to connect to server")?;
    write_frame(&mut stream, req)?;
    let resp: DaemonMessage = read_frame(&mut stream)?;
    Ok(resp)
}
