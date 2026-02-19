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
