//! `AsyncWrite` over `&mut BytesMut`. Used to capture bytes that would otherwise
//! be proxied to a client socket — feeds `Server::recv` and lets the caller
//! inspect or cache the full response without touching the client connection.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use tokio::io::AsyncWrite;

pub struct BufferingWriter<'a> {
    buf: &'a mut BytesMut,
    max_len: Option<usize>,
}

impl<'a> BufferingWriter<'a> {
    pub fn new(buf: &'a mut BytesMut) -> Self {
        Self { buf, max_len: None }
    }

    pub fn capped(buf: &'a mut BytesMut, max_len: usize) -> Self {
        Self {
            buf,
            max_len: Some(max_len),
        }
    }
}

impl<'a> AsyncWrite for BufferingWriter<'a> {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        if let Some(max_len) = this.max_len {
            let next_len = match this.buf.len().checked_add(buf.len()) {
                Some(next_len) => next_len,
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "buffering writer length overflow",
                    )))
                }
            };
            if next_len > max_len {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("buffering writer cap exceeded: max {max_len} bytes"),
                )));
            }
        }
        this.buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn write_all_then_flush_appends_to_buffer() {
        let mut buf = BytesMut::new();
        {
            let mut writer = BufferingWriter::new(&mut buf);
            writer.write_all(b"hello").await.unwrap();
            writer.flush().await.unwrap();
        }
        assert_eq!(&buf[..], b"hello");
    }

    #[tokio::test]
    async fn multi_chunk_writes_concatenate_in_order() {
        let mut buf = BytesMut::new();
        {
            let mut writer = BufferingWriter::new(&mut buf);
            writer.write_all(b"foo").await.unwrap();
            writer.write_all(b"bar").await.unwrap();
            writer.write_all(b"baz").await.unwrap();
        }
        assert_eq!(&buf[..], b"foobarbaz");
    }

    #[tokio::test]
    async fn shutdown_is_idempotent_and_preserves_buffer() {
        let mut buf = BytesMut::new();
        {
            let mut writer = BufferingWriter::new(&mut buf);
            writer.write_all(b"payload").await.unwrap();
            writer.shutdown().await.unwrap();
            writer.shutdown().await.unwrap();
        }
        assert_eq!(&buf[..], b"payload");
    }

    #[tokio::test]
    async fn empty_write_keeps_buffer_empty() {
        let mut buf = BytesMut::new();
        {
            let mut writer = BufferingWriter::new(&mut buf);
            writer.write_all(b"").await.unwrap();
        }
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn capped_writer_rejects_oversize_before_append() {
        let mut buf = BytesMut::from(&b"abcd"[..]);
        {
            let mut writer = BufferingWriter::capped(&mut buf, 5);
            let err = writer
                .write_all(b"ef")
                .await
                .expect_err("write beyond cap must fail");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
        assert_eq!(&buf[..], b"abcd", "rejected bytes must not be appended");
    }
}
