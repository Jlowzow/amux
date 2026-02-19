use std::os::unix::net::UnixStream;

use anyhow::Context;

use crate::common::{read_frame, socket_path, write_frame, Request, Response};

/// Connect to the server and return the stream.
pub fn connect() -> anyhow::Result<UnixStream> {
    let path = socket_path();
    UnixStream::connect(&path)
        .with_context(|| format!("failed to connect to server at {}", path.display()))
}

/// Send a request and read the response.
pub fn request(req: &Request) -> anyhow::Result<Response> {
    let mut stream = connect().context("is the server running? try: amux start-server")?;
    write_frame(&mut stream, req)?;
    let resp: Response = read_frame(&mut stream)?;
    Ok(resp)
}
