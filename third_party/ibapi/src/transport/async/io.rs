//! Async stream abstraction for `AsyncConnection` / `AsyncTcpMessageBus`.
//!
//! Mirrors the sync `transport::sync::{Io, Reconnect, Stream}` triple, but
//! method-async via `#[async_trait]`. Frame-level: `read_message` returns the
//! already-unframed body so callers don't repeat the length-prefix dance.

use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::task::{ready, Poll};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::errors::Error;

#[async_trait]
pub(crate) trait AsyncIo {
    async fn read_message(&self) -> Result<Vec<u8>, Error>;
    async fn write_all(&self, buf: &[u8]) -> Result<(), Error>;
    async fn close(&self);
}

#[async_trait]
pub(crate) trait AsyncReconnect {
    async fn reconnect(&self) -> Result<(), Error>;
    async fn sleep(&self, duration: Duration);
}

pub(crate) trait AsyncStream: AsyncIo + AsyncReconnect + Send + Sync + 'static + std::fmt::Debug {}

/// Production async stream over `tokio::net::TcpStream`. Holds the split halves
/// behind `Mutex` so reads and writes can run concurrently from the dispatcher
/// task and the request senders.
#[derive(Debug)]
pub(crate) struct AsyncTcpSocket {
    reader: Mutex<Option<OwnedReadHalf>>,
    writer: StdMutex<Option<OwnedWriteHalf>>,
    write_order: Mutex<()>,
    connection_url: String,
    tcp_no_delay: bool,
}

impl AsyncTcpSocket {
    pub async fn connect(address: &str, tcp_no_delay: bool) -> Result<Self, Error> {
        let stream = TcpStream::connect(address).await?;
        stream.set_nodelay(tcp_no_delay)?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: Mutex::new(Some(read_half)),
            writer: StdMutex::new(Some(write_half)),
            write_order: Mutex::new(()),
            connection_url: address.to_string(),
            tcp_no_delay,
        })
    }
}

#[async_trait]
impl AsyncIo for AsyncTcpSocket {
    async fn read_message(&self) -> Result<Vec<u8>, Error> {
        let mut reader = self.reader.lock().await;
        let reader = reader.as_mut().ok_or(Error::Shutdown)?;
        let mut length_bytes = [0u8; 4];
        reader.read_exact(&mut length_bytes).await?;
        let message_length = u32::from_be_bytes(length_bytes) as usize;
        let mut data = vec![0u8; message_length];
        reader.read_exact(&mut data).await?;
        Ok(data)
    }

    async fn write_all(&self, mut remaining: &[u8]) -> Result<(), Error> {
        // Serialize whole frames, but borrow the socket only during a poll.
        // Closing it must not depend on the owner polling a stalled write again.
        let _frame = self.write_order.lock().await;
        poll_fn(|cx| {
            let mut writer = self.writer.lock()?;
            let writer = writer.as_mut().ok_or(Error::Shutdown)?;
            while !remaining.is_empty() {
                let n = ready!(Pin::new(&mut *writer).poll_write(cx, remaining))?;
                if n == 0 {
                    return Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero).into()));
                }
                remaining = &remaining[n..];
            }
            Poll::Ready(Ok(ready!(Pin::new(writer).poll_flush(cx))?))
        })
        .await
    }

    async fn close(&self) {
        self.reader.lock().await.take();
        self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    }
}

#[async_trait]
impl AsyncReconnect for AsyncTcpSocket {
    async fn reconnect(&self) -> Result<(), Error> {
        let stream = TcpStream::connect(&self.connection_url).await?;
        stream.set_nodelay(self.tcp_no_delay)?;
        let (new_reader, new_writer) = stream.into_split();
        let _frame = self.write_order.lock().await;
        *self.reader.lock().await = Some(new_reader);
        *self.writer.lock()? = Some(new_writer);
        Ok(())
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await
    }
}

impl AsyncStream for AsyncTcpSocket {}
