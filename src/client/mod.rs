pub mod attach;

use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::Context;

use crate::common;
use crate::protocol::codec::{read_frame, write_frame};
use crate::protocol::messages::{ClientMessage, DaemonMessage};

/// Deadline applied to simple request/response RPCs so a hung daemon can
/// never freeze a client indefinitely. Streaming connections (attach,
/// follow, watch) upgrade to an async tokio stream and don't use this.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to the daemon and return the stream. No timeouts are applied;
/// callers that need deadlines must set them explicitly (see `request`).
pub fn connect() -> anyhow::Result<UnixStream> {
    let path = common::socket_path();
    UnixStream::connect(&path)
        .with_context(|| format!("failed to connect to server at {}", path.display()))
}

/// Send a request and read the response (sync, for simple commands).
///
/// Applies `REQUEST_TIMEOUT` to reads and writes so a hung or unresponsive
/// daemon produces a clear error instead of hanging the client forever.
pub fn request(req: &ClientMessage) -> anyhow::Result<DaemonMessage> {
    let mut stream = connect().context("is the server running? try: amux start-server")?;
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .context("failed to set read timeout")?;
    stream
        .set_write_timeout(Some(REQUEST_TIMEOUT))
        .context("failed to set write timeout")?;
    do_request(&mut stream, req)
}

/// Core request/response cycle, shared by `request` and tests.
fn do_request(stream: &mut UnixStream, req: &ClientMessage) -> anyhow::Result<DaemonMessage> {
    write_frame(stream, req).map_err(|e| map_io_timeout(e, "write"))?;
    read_frame(stream).map_err(|e| map_io_timeout(e, "read"))
}

/// If `e` wraps a socket timeout (`WouldBlock` or `TimedOut`), rewrite it
/// into a clear, actionable message; otherwise pass it through.
fn map_io_timeout(e: anyhow::Error, op: &str) -> anyhow::Error {
    let is_timeout = e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            })
            .unwrap_or(false)
    });
    if is_timeout {
        anyhow::anyhow!(
            "daemon did not respond ({} timed out after {}s) — it may be hung; \
             try: amux kill-server && amux start-server",
            op,
            REQUEST_TIMEOUT.as_secs()
        )
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    fn unique_sock_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "amux-test-{}-{}",
            tag,
            std::process::id(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.join(format!("{}.sock", nanos))
    }

    /// Spawn a listener that accepts one connection, drains writes, and
    /// never sends a response. Simulates a hung daemon.
    fn silent_listener(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        let _ = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the client's request so its write doesn't block,
                // but never reply.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
    }

    #[test]
    fn do_request_times_out_on_unresponsive_peer() {
        let sock = unique_sock_path("hung");
        silent_listener(&sock);
        // Give the listener a moment to bind.
        std::thread::sleep(Duration::from_millis(50));

        let mut stream = UnixStream::connect(&sock).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        let start = Instant::now();
        let result = do_request(&mut stream, &ClientMessage::Ping);
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout error");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("did not respond") || err_msg.contains("hung"),
            "expected hung-daemon message, got: {}",
            err_msg
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "request should fail fast, took {:?}",
            elapsed
        );

        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn do_request_returns_response_when_peer_replies() {
        use crate::protocol::codec::{read_frame, write_frame};

        let sock = unique_sock_path("reply");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request, reply with Pong.
                let _: ClientMessage = read_frame(&mut stream).unwrap();
                write_frame(&mut stream, &DaemonMessage::Pong).unwrap();
            }
        });
        std::thread::sleep(Duration::from_millis(50));

        let mut stream = UnixStream::connect(&sock).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let resp = do_request(&mut stream, &ClientMessage::Ping).unwrap();
        assert!(matches!(resp, DaemonMessage::Pong));

        let _ = std::fs::remove_file(&sock);
    }
}
