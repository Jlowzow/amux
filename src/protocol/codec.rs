use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

const MAX_FRAME_SIZE: usize = 1024 * 1024; // 1MB

/// Write a length-prefixed bincode frame to a sync writer.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> anyhow::Result<()> {
    let data = bincode::serialize(msg)?;
    let len = (data.len() as u32).to_be_bytes();
    w.write_all(&len)?;
    w.write_all(&data)?;
    w.flush()?;
    Ok(())
}

/// Read a length-prefixed bincode frame from a sync reader.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let msg = bincode::deserialize(&buf)?;
    Ok(msg)
}

/// Write a length-prefixed bincode frame to an async writer.
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

/// Read a length-prefixed bincode frame from an async reader.
#[allow(dead_code)]
pub async fn read_frame_async<T: for<'de> Deserialize<'de>>(
    r: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let msg = bincode::deserialize(&buf)?;
    Ok(msg)
}

/// Try to read a frame, returning None on EOF/disconnect.
pub async fn try_read_frame_async<T: for<'de> Deserialize<'de>>(
    r: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> Option<anyhow::Result<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return None,
        Err(e) => return Some(Err(e.into())),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Some(Err(anyhow::anyhow!("frame too large: {} bytes", len)));
    }
    let mut buf = vec![0u8; len];
    match r.read_exact(&mut buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return None,
        Err(e) => return Some(Err(e.into())),
    }
    match bincode::deserialize(&buf) {
        Ok(msg) => Some(Ok(msg)),
        Err(e) => Some(Err(e.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::{ClientMessage, DaemonMessage, SessionInfo};

    #[test]
    fn test_roundtrip_ping() {
        let msg = ClientMessage::Ping;
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: ClientMessage = read_frame(&mut &buf[..]).unwrap();
        assert!(matches!(decoded, ClientMessage::Ping));
    }

    #[test]
    fn test_roundtrip_create_session() {
        let msg = ClientMessage::CreateSession {
            name: Some("test-session".to_string()),
            command: vec!["bash".to_string(), "-c".to_string(), "echo hi".to_string()],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: ClientMessage = read_frame(&mut &buf[..]).unwrap();
        match decoded {
            ClientMessage::CreateSession { name, command } => {
                assert_eq!(name, Some("test-session".to_string()));
                assert_eq!(command, vec!["bash", "-c", "echo hi"]);
            }
            _ => panic!("expected CreateSession"),
        }
    }

    #[test]
    fn test_roundtrip_session_list() {
        let msg = DaemonMessage::SessionList(vec![
            SessionInfo {
                name: "s1".to_string(),
                command: "bash".to_string(),
                pid: 1234,
                alive: true,
                created_at: "2026-02-20T12:00:00Z".to_string(),
                uptime_secs: 60,
            },
            SessionInfo {
                name: "s2".to_string(),
                command: "vim".to_string(),
                pid: 5678,
                alive: false,
                created_at: "2026-02-20T11:00:00Z".to_string(),
                uptime_secs: 3660,
            },
        ]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: DaemonMessage = read_frame(&mut &buf[..]).unwrap();
        match decoded {
            DaemonMessage::SessionList(list) => {
                assert_eq!(list.len(), 2);
                assert_eq!(list[0].name, "s1");
                assert!(list[0].alive);
                assert_eq!(list[1].name, "s2");
                assert!(!list[1].alive);
            }
            _ => panic!("expected SessionList"),
        }
    }

    #[test]
    fn test_roundtrip_output() {
        let data = b"hello terminal output\x1b[31mred\x1b[0m".to_vec();
        let msg = DaemonMessage::Output(data.clone());
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: DaemonMessage = read_frame(&mut &buf[..]).unwrap();
        match decoded {
            DaemonMessage::Output(decoded_data) => {
                assert_eq!(decoded_data, data);
            }
            _ => panic!("expected Output"),
        }
    }

    #[test]
    fn test_roundtrip_send_input() {
        let msg = ClientMessage::SendInput {
            name: "mysession".to_string(),
            data: b"ls -la".to_vec(),
            newline: true,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: ClientMessage = read_frame(&mut &buf[..]).unwrap();
        match decoded {
            ClientMessage::SendInput {
                name,
                data,
                newline,
            } => {
                assert_eq!(name, "mysession");
                assert_eq!(data, b"ls -la");
                assert!(newline);
            }
            _ => panic!("expected SendInput"),
        }
    }

    #[test]
    fn test_roundtrip_send_input_literal() {
        let msg = ClientMessage::SendInput {
            name: "sess".to_string(),
            data: b"partial text".to_vec(),
            newline: false,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: ClientMessage = read_frame(&mut &buf[..]).unwrap();
        match decoded {
            ClientMessage::SendInput {
                name,
                data,
                newline,
            } => {
                assert_eq!(name, "sess");
                assert_eq!(data, b"partial text");
                assert!(!newline);
            }
            _ => panic!("expected SendInput"),
        }
    }

    #[test]
    fn test_roundtrip_input_sent() {
        let msg = DaemonMessage::InputSent;
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: DaemonMessage = read_frame(&mut &buf[..]).unwrap();
        assert!(matches!(decoded, DaemonMessage::InputSent));
    }

    #[test]
    fn test_frame_length_prefix() {
        let msg = ClientMessage::Ping;
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        // First 4 bytes are the length prefix (big-endian u32).
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(len, buf.len() - 4);
    }

    #[tokio::test]
    async fn test_async_roundtrip() {
        let msg = ClientMessage::Attach {
            name: "test".to_string(),
            cols: 80,
            rows: 24,
        };
        let mut buf = Vec::new();
        write_frame_async(&mut buf, &msg).await.unwrap();
        let mut cursor = &buf[..];
        let decoded: ClientMessage = read_frame_async(&mut cursor).await.unwrap();
        match decoded {
            ClientMessage::Attach { name, cols, rows } => {
                assert_eq!(name, "test");
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            _ => panic!("expected Attach"),
        }
    }

    #[tokio::test]
    async fn test_try_read_eof() {
        let empty: &[u8] = &[];
        let mut cursor = empty;
        let result: Option<anyhow::Result<ClientMessage>> =
            try_read_frame_async(&mut cursor).await;
        assert!(result.is_none());
    }
}
