use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub(super) async fn send_proxy_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
) -> std::io::Result<()> {
    stream
        .write_all(
            format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                .as_bytes(),
        )
        .await
}

pub(super) async fn copy_exact_with_idle_timeout<R, W>(
    reader: &mut R,
    writer: &mut W,
    mut remaining: u64,
    idle_timeout: Duration,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 16 * 1024];
    while remaining > 0 {
        let amount = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = tokio::time::timeout(idle_timeout, reader.read(&mut buffer[..amount]))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle timeout"))??;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request body ended early",
            ));
        }
        tokio::time::timeout(idle_timeout, writer.write_all(&buffer[..read]))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle timeout"))??;
        remaining = remaining.saturating_sub(read as u64);
    }
    Ok(())
}

pub(super) async fn copy_until_eof_with_idle_timeout<R, W>(
    reader: &mut R,
    writer: &mut W,
    idle_timeout: Duration,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = tokio::time::timeout(idle_timeout, reader.read(&mut buffer))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle timeout"))??;
        if read == 0 {
            writer.shutdown().await?;
            return Ok(());
        }
        tokio::time::timeout(idle_timeout, writer.write_all(&buffer[..read]))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle timeout"))??;
    }
}

pub(super) async fn tunnel_with_idle_timeout(
    client: TcpStream,
    upstream: TcpStream,
    idle_timeout: Duration,
) -> std::io::Result<()> {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let mut client_buffer = [0_u8; 16 * 1024];
    let mut upstream_buffer = [0_u8; 16 * 1024];
    let mut client_closed = false;
    let mut upstream_closed = false;
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);

    while !client_closed || !upstream_closed {
        tokio::select! {
            read = client_read.read(&mut client_buffer), if !client_closed => {
                let read = read?;
                if read == 0 {
                    client_closed = true;
                    upstream_write.shutdown().await?;
                } else {
                    upstream_write.write_all(&client_buffer[..read]).await?;
                    idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                }
            }
            read = upstream_read.read(&mut upstream_buffer), if !upstream_closed => {
                let read = read?;
                if read == 0 {
                    upstream_closed = true;
                    client_write.shutdown().await?;
                } else {
                    client_write.write_all(&upstream_buffer[..read]).await?;
                    idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                }
            }
            _ = &mut idle => {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "idle timeout"));
            }
        }
    }
    Ok(())
}
