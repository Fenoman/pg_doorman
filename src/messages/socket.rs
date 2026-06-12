use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::errors::Error;
use crate::errors::Error::ProxyTimeout;
use crate::messages::{CURRENT_MEMORY, MAX_MESSAGE_SIZE};

/// Default capacity for a freshly allocated reusable read buffer.
const REUSE_BUF_DEFAULT_CAPACITY: usize = 16 * 1024;

/// `BytesMut::split()` keeps the backing allocation alive through its Arc.
/// When the caller drops the returned `BytesMut`, the reusable buffer
/// reclaims that capacity for the next read, including a multi-MiB region
/// allocated for one oversized query. Without this cap, `buf` retains the
/// largest allocation it has served. On a pool with thousands of clients,
/// each occasional megabyte-sized INSERT turns into a per-connection leak.
const REUSE_BUF_SHRINK_THRESHOLD: usize = 256 * 1024;

/// Drop an oversized reusable read buffer before the next read. Without it,
/// `reserve()` would inherit the previous multi-MiB allocation. The
/// steady-state path (capacity within threshold) falls through unchanged.
#[inline]
fn shrink_reuse_buf(buf: &mut BytesMut) {
    if buf.capacity() > REUSE_BUF_SHRINK_THRESHOLD {
        *buf = BytesMut::with_capacity(REUSE_BUF_DEFAULT_CAPACITY);
    }
}

const ADMIN_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

async fn read_message_body_into_reuse_buf<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    code: u8,
    body_len: usize,
) -> Result<(), Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    let mut remaining = body_len;
    while remaining > 0 {
        let n = match (&mut *stream).take(remaining as u64).read_buf(buf).await {
            Ok(0) => {
                return Err(Error::SocketError(format!(
                    "EOF reading message data from socket - Code: {code:?}, expected {body_len} bytes, got {} more",
                    body_len - remaining
                )));
            }
            Ok(n) => n,
            Err(err) => {
                return Err(Error::SocketError(format!(
                    "Error reading message data from socket - Code: {code:?}, Error: {err:?}"
                )));
            }
        };
        remaining -= n;
    }
    Ok(())
}

/// Write all data in the buffer to the TcpStream.
pub async fn write_all<S>(stream: &mut S, buf: BytesMut) -> Result<(), Error>
where
    S: tokio::io::AsyncWrite + std::marker::Unpin,
{
    match stream.write_all(&buf).await {
        Ok(_) => Ok(()),
        Err(err) => Err(Error::SocketError(format!(
            "Error writing to socket - Error: {err:?}"
        ))),
    }
}

/// Write all the data in the buffer to the TcpStream, write owned half (see mpsc).
pub async fn write_all_half<S>(stream: &mut S, buf: &BytesMut) -> Result<(), Error>
where
    S: tokio::io::AsyncWrite + std::marker::Unpin,
{
    match timeout(ADMIN_RESPONSE_WRITE_TIMEOUT, stream.write_all(buf)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            record_admin_response_write_error("write", "io");
            log::error!("Failed to write admin response: {err}");
            return Err(Error::SocketError(format!(
                "Error writing admin response to socket: {err:?}"
            )));
        }
        Err(_) => {
            record_admin_response_write_error("write", "timeout");
            log::warn!("Timed out writing admin response after {ADMIN_RESPONSE_WRITE_TIMEOUT:?}");
            return Err(Error::SocketError(
                "admin response write timed out".to_string(),
            ));
        }
    }

    match timeout(ADMIN_RESPONSE_WRITE_TIMEOUT, stream.flush()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => {
            record_admin_response_write_error("flush", "io");
            log::error!("Failed to flush admin response: {err}");
            Err(Error::SocketError(format!(
                "Error flushing admin response to socket: {err:?}"
            )))
        }
        Err(_) => {
            record_admin_response_write_error("flush", "timeout");
            log::warn!("Timed out flushing admin response after {ADMIN_RESPONSE_WRITE_TIMEOUT:?}");
            Err(Error::SocketError(
                "admin response flush timed out".to_string(),
            ))
        }
    }
}

fn record_admin_response_write_error(stage: &'static str, reason: &'static str) {
    crate::web::metrics::ADMIN_RESPONSE_WRITE_ERRORS_TOTAL
        .with_label_values(&[stage, reason])
        .inc();
}

/// Write all the data in the buffer to the TcpStream and flush the stream.
pub async fn write_all_flush<S>(stream: &mut S, buf: &[u8]) -> Result<(), Error>
where
    S: tokio::io::AsyncWrite + std::marker::Unpin,
{
    match stream.write_all(buf).await {
        Ok(_) => match stream.flush().await {
            Ok(_) => Ok(()),
            Err(err) => Err(Error::SocketError(format!(
                "Error flushing socket: {err:?}"
            ))),
        },
        Err(err) => Err(Error::SocketError(format!(
            "Error writing to socket: {err:?}"
        ))),
    }
}

/// Write all data and flush the stream under a single caller-provided deadline
/// for each operation.
pub async fn write_all_flush_timeout<S>(
    stream: &mut S,
    buf: &[u8],
    duration: Duration,
) -> Result<(), Error>
where
    S: tokio::io::AsyncWrite + std::marker::Unpin,
{
    match timeout(duration, stream.write_all(buf)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            return Err(Error::SocketError(format!(
                "Error writing to socket: {err:?}"
            )))
        }
        Err(_) => return Err(ProxyTimeout),
    }

    match timeout(duration, stream.flush()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(Error::SocketError(format!(
            "Error flushing socket: {err:?}"
        ))),
        Err(_) => Err(ProxyTimeout),
    }
}

/// Read message header.
pub async fn read_message_header<S>(stream: &mut S) -> Result<(u8, i32), Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    let code = match stream.read_u8().await {
        Ok(code) => code,
        Err(err) => {
            return Err(Error::SocketError(format!(
                "Error reading message code from socket - Error {err:?}"
            )))
        }
    };
    let len = match stream.read_i32().await {
        Ok(len) => len,
        Err(err) => {
            return Err(Error::SocketError(format!(
                "Error reading message len from socket - Code: {code:?}, Error: {err:?}"
            )))
        }
    };

    Ok((code, len))
}

/// Read message data.
pub async fn read_message_data<S>(stream: &mut S, code: u8, len: i32) -> Result<BytesMut, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    if len < 4 {
        return Err(Error::ProtocolSyncError(format!(
            "Message length is too small: {len}"
        )));
    }

    // Use `>=` (not strict `>`) so a malformed claim of EXACTLY
    // `MAX_MESSAGE_SIZE` (256MB) is also rejected. A length of exactly the
    // limit is never legitimate PG protocol traffic - and allocating
    // `BytesMut::with_capacity(MAX_MESSAGE_SIZE + 1)` plus `buf.resize(...)`
    // for it would synchronously commit 256MB of zeroed pages, blocking
    // the runtime and starving any concurrent client of memory budget
    // (CURRENT_MEMORY would hit `max_memory_usage` exactly, causing
    // every parallel `read_message` to bail with `CurrentMemoryUsage`).
    // A standalone fuzzer that sends `[Q, 0x10, 0x00, 0x00, 0x00]`
    // exercises this exact path.
    if len >= MAX_MESSAGE_SIZE {
        return Err(Error::ProtocolSyncError(format!(
            "Message length is too large: {len}"
        )));
    }

    let total_len = len as usize + 1; // code(1) + len(4) + data
    let mut buf = BytesMut::with_capacity(total_len);
    buf.put_u8(code);
    buf.put_i32(len);
    // earlier used
    // `buf.resize(total_len, 0)` which synchronously zero-fills up to
    // ~256 MiB per call. F7 replaced that with an unsafe `&mut [u8]`
    // slice over uninitialised memory - which is immediate UB per the
    // Rust reference (producing a `&mut [u8]` that aliases uninit
    // bytes is undefined behaviour even if no one reads it). Now we
    // use the safe `read_buf` API via `tokio::io::AsyncReadExt::read_buf`
    // chained until full, which uses `BufMut` semantics and tolerates
    // uninitialised capacity. This keeps the F7 zero-fill avoidance
    // while being sound.
    let body_len = total_len - 5;
    let mut remaining = body_len;
    while remaining > 0 {
        let n = match stream.read_buf(&mut buf).await {
            Ok(0) => {
                return Err(Error::SocketError(format!(
                    "EOF reading message data from socket - Code: {code:?}, expected {body_len} bytes, got {} more",
                    body_len - remaining
                )));
            }
            Ok(n) => n,
            Err(err) => {
                return Err(Error::SocketError(format!(
                    "Error reading message data from socket - Code: {code:?}, Error: {err:?}"
                )))
            }
        };
        remaining = remaining.saturating_sub(n);
    }
    Ok(buf)
}

/// Read a message body when the caller has already consumed the message header.
/// This preserves the same global memory-budget guard used by `read_message()`
/// for startup paths that need to inspect the frame code before reading a body.
pub async fn read_message_data_with_memory_limit<S>(
    stream: &mut S,
    code: u8,
    len: i32,
    max_memory_usage: u64,
) -> Result<BytesMut, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    if !(4..MAX_MESSAGE_SIZE).contains(&len) {
        return read_message_data(stream, code, len).await;
    }

    let _reservation = MemoryReservation::reserve(len as i64, max_memory_usage)?;
    read_message_data(stream, code, len).await
}

/// RAII guard for the `CURRENT_MEMORY` budget. The previous
/// shape `fetch_add -> await -> fetch_sub` leaked the reservation on
/// task cancellation (caller drop during `read_message_data().await`),
/// permanently inflating `CURRENT_MEMORY` until it tripped the limit
/// for every inbound message. The pooler then appeared OOM-locked
/// while real RSS was fine. `Drop` runs on cancellation too, so the
/// guard restores the counter unconditionally.
///
/// every `read_message*` invocation took
/// two `fetch_add`/`fetch_sub` Relaxed atomics on a single global
/// `AtomicI64`. With N workers reading at OLTP rates this is a
/// cache-line ping-pong source visible as `__aarch64_ldadd8_rel` 0.48 %
/// on S5 and 0.13-0.23 % across all scenarios. Small messages
/// (<4 KiB - DataRow tail, BindComplete, ParseComplete,
/// CommandComplete, ReadyForQuery, ParameterStatus) cannot
/// individually move the budget needle: at the default 128 MiB
/// `max_memory_usage` even 32k concurrent small messages stays under
/// the cap. The fast path encodes "no reservation" as `len == 0` and
/// the Drop guard skips the atomic store for the sentinel - preserves
/// the cancellation-safe accounting on large messages (COPY, BLOBs,
/// outsized DataRow) while removing the contention for the 95 % case.
struct MemoryReservation(i64);

/// messages below this size bypass the global budget
/// atomics entirely. Picked at 4 KiB to cover virtually every
/// extended-protocol frame (typical Bind/Execute body ≤ a few hundred
/// bytes, DataRow up to a row, ReadyForQuery / ParameterStatus < 20 B)
/// while keeping the budget meaningful for COPY chunks and bulk
/// DataRow batches. Aggregate undercount at this threshold is bounded
/// by `concurrent_inflight × 4 KiB` - well under any operator-set
/// `max_memory_usage`.
const MEMORY_RESERVATION_SKIP_THRESHOLD: i64 = 4096;

impl MemoryReservation {
    /// Reserve `len` bytes against the global budget. Returns
    /// `Err(CurrentMemoryUsage)` immediately if the reservation would
    /// exceed `max_memory_usage` - the failed fetch_add is rolled back
    /// before returning.
    ///
    /// defensive bounds - reject negative or zero `len`
    /// at the API boundary so a future caller cannot accidentally
    /// budget against a malformed message. saturating_add catches the
    /// theoretical i64 overflow (would require ~36 billion concurrent
    /// in-flight reservations of MAX_MESSAGE_SIZE, but explicit
    /// checked-add removes the "works by accident" UB foot-gun).
    fn reserve(len: i64, max_memory_usage: u64) -> Result<Self, Error> {
        if len <= 0 {
            return Err(Error::ParseBytesError(format!(
                "MemoryReservation::reserve called with non-positive len={len}"
            )));
        }
        // the small-message fast path: small messages skip the global atomic
        // entirely. Sentinel `MemoryReservation(0)` so Drop is a no-op.
        if len < MEMORY_RESERVATION_SKIP_THRESHOLD {
            return Ok(MemoryReservation(0));
        }
        let prev = CURRENT_MEMORY.fetch_add(len, Ordering::Relaxed);
        // Saturating add so a negative `prev` (transient under storms)
        // never wraps as_u64 negative to near-MAX.
        let new_total_u64 = prev.saturating_add(len).max(0) as u64;
        if new_total_u64 > max_memory_usage {
            CURRENT_MEMORY.fetch_sub(len, Ordering::Relaxed);
            return Err(Error::CurrentMemoryUsage);
        }
        Ok(MemoryReservation(len))
    }
}

impl Drop for MemoryReservation {
    #[inline]
    fn drop(&mut self) {
        // skip the atomic store on the sentinel.
        if self.0 != 0 {
            CURRENT_MEMORY.fetch_sub(self.0, Ordering::Relaxed);
        }
    }
}

#[inline]
pub async fn read_message<S>(stream: &mut S, max_memory_usage: u64) -> Result<BytesMut, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    let (code, len) = read_message_header(stream).await?;
    // RAII so the reservation is released even on task cancel.
    let _reservation = MemoryReservation::reserve(len as i64, max_memory_usage)?;
    read_message_data(stream, code, len).await
}

/// Read a message into a reusable buffer. Returns the owned `BytesMut` via
/// `split()` while the backing capacity stays in `buf` for the next call.
///
/// Amortized allocation cost: roughly one heap alloc per
/// `REUSE_BUF_DEFAULT_CAPACITY / msg_size` messages. `split()` hands off the
/// filled region; `reserve()` reuses any remaining capacity in the same
/// allocation until exhausted. A buffer that grew past
/// `REUSE_BUF_SHRINK_THRESHOLD` is dropped before the next read, so a single
/// oversized message does not pin its allocation across the connection.
#[inline]
pub async fn read_message_reuse<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    max_memory_usage: u64,
) -> Result<BytesMut, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    let (code, len) = read_message_header(stream).await?;

    if len < 4 {
        return Err(Error::ProtocolSyncError(format!(
            "Message length is too small: {len}"
        )));
    }
    // See the matching comment in `read_message_data` - `>=` (not `>`)
    // catches the fuzzer-shaped "exactly MAX_MESSAGE_SIZE" claim before
    // any allocation.
    if len >= MAX_MESSAGE_SIZE {
        return Err(Error::ProtocolSyncError(format!(
            "Message length is too large: {len}"
        )));
    }

    // RAII reservation released even on task cancellation.
    let _reservation = MemoryReservation::reserve(len as i64, max_memory_usage)?;

    let total_len = len as usize + 1;
    shrink_reuse_buf(buf);
    buf.clear();
    buf.reserve(total_len);
    buf.put_u8(code);
    buf.put_i32(len);
    let body_len = total_len - 5;
    match read_message_body_into_reuse_buf(stream, buf, code, body_len).await {
        Ok(()) => Ok(buf.split()),
        Err(err) => {
            buf.truncate(5);
            Err(err)
        }
    }
}

#[inline]
pub async fn read_message_reuse_cancel_safe<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    max_memory_usage: u64,
) -> Result<BytesMut, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    if buf.is_empty() {
        shrink_reuse_buf(buf);
    }

    while buf.len() < 5 {
        let need = 5 - buf.len();
        match (&mut *stream).take(need as u64).read_buf(buf).await {
            Ok(0) => {
                return Err(Error::SocketError(format!(
                    "EOF reading message header from socket - expected 5 bytes, got {}",
                    buf.len()
                )));
            }
            Ok(_) => {}
            Err(err) => {
                return Err(Error::SocketError(format!(
                    "Error reading message header from socket - Error: {err:?}"
                )));
            }
        }
    }

    let code = buf[0];
    let len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    if len < 4 {
        return Err(Error::ProtocolSyncError(format!(
            "Message length is too small: {len}"
        )));
    }
    if len >= MAX_MESSAGE_SIZE {
        return Err(Error::ProtocolSyncError(format!(
            "Message length is too large: {len}"
        )));
    }

    let _reservation = MemoryReservation::reserve(len as i64, max_memory_usage)?;

    let total_len = len as usize + 1;
    if buf.len() > total_len {
        return Err(Error::ProtocolSyncError(format!(
            "Reusable read buffer already contains {} bytes for message length {total_len}",
            buf.len()
        )));
    }
    buf.reserve(total_len - buf.len());

    while buf.len() < total_len {
        let need = total_len - buf.len();
        match (&mut *stream).take(need as u64).read_buf(buf).await {
            Ok(0) => {
                let expected = total_len - 5;
                let got = buf.len().saturating_sub(5);
                buf.truncate(5);
                return Err(Error::SocketError(format!(
                    "EOF reading message data from socket - Code: {code:?}, expected {expected} bytes, got {got} more"
                )));
            }
            Ok(_) => {}
            Err(err) => {
                buf.truncate(5);
                return Err(Error::SocketError(format!(
                    "Error reading message data from socket - Code: {code:?}, Error: {err:?}"
                )));
            }
        }
    }

    Ok(buf.split())
}

/// Read message body into a reusable buffer when header is already consumed.
/// Used by server recv() loop where read_message_header() is called separately.
/// Same amortized allocation semantics as read_message_reuse, but skips header read.
#[inline]
pub async fn read_message_body_reuse<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    code: u8,
    len: i32,
) -> Result<BytesMut, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    if len < 4 {
        return Err(Error::ProtocolSyncError(format!(
            "Message length is too small: {len}"
        )));
    }

    let total_len = len as usize + 1;
    shrink_reuse_buf(buf);
    buf.clear();
    buf.reserve(total_len);
    buf.put_u8(code);
    buf.put_i32(len);
    let body_len = total_len - 5;
    match read_message_body_into_reuse_buf(stream, buf, code, body_len).await {
        Ok(()) => Ok(buf.split()),
        Err(err) => {
            buf.truncate(5);
            Err(err)
        }
    }
}

/// Copy data from one stream to another with a timeout.
///
/// `copied` is updated as bytes flow so the caller can record the
/// actual amount forwarded even when the underlying copy fails or
/// times out partway through. On a clean copy `copied == len` on
/// return; on a timeout-driven cancellation the future is dropped and
/// `copied` reflects how far the copy got before tokio aborted it.
pub async fn proxy_copy_data_with_timeout<R, W>(
    duration: tokio::time::Duration,
    read: &mut R,
    write: &mut W,
    len: usize,
    copied: &mut usize,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + std::marker::Unpin,
    W: tokio::io::AsyncWrite + std::marker::Unpin,
{
    match timeout(duration, proxy_copy_data(read, write, len, copied)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(ProxyTimeout),
    }
}

/// Copy data from one stream to another.
///
/// The caller passes `copied` initialized to zero; the function bumps
/// it as each chunk lands in the writer. On `Err` the value reflects
/// what actually reached the wire before the failure, which the
/// streaming-byte counter relies on to avoid overstating large-message
/// throughput on partial reads or writer disconnects.
pub async fn proxy_copy_data<R, W>(
    read: &mut R,
    write: &mut W,
    len: usize,
    copied: &mut usize,
) -> Result<(), Error>
where
    R: tokio::io::AsyncRead + std::marker::Unpin,
    W: tokio::io::AsyncWrite + std::marker::Unpin,
{
    // 64 KiB chunk: a single >1MB frame streams to the client in far fewer
    // read/write syscalls than the previous 4 KiB chunk.
    const MAX_BUFFER_CHUNK: usize = 65536;
    let mut bytes_remained = len;
    let mut bytes_readed: usize;
    let mut buffer_size: usize = MAX_BUFFER_CHUNK;
    if buffer_size > len {
        buffer_size = len
    }
    // Heap buffer sized to the chunk so a 64 KiB array does not live in this
    // future on the stack. buffer_size only shrinks below, so the allocation
    // stays large enough for every later slice.
    let mut buffer = vec![0u8; buffer_size];
    loop {
        // read.
        match read.read(&mut buffer[..buffer_size]).await {
            Ok(n) => bytes_readed = n,
            Err(err) => {
                return Err(Error::SocketError(format!(
                    "Error reading from socket: {err:?}"
                )))
            }
        };
        if bytes_readed == 0 {
            return Err(Error::SocketError(
                "Error reading from socket: connection closed".to_string(),
            ));
        }

        // Write in a partial-aware loop so `copied` reflects bytes
        // that actually reached the writer even when the underlying
        // sink fails halfway through a chunk. `write_all` would hide
        // that signal because it returns Err without saying how much
        // of the buffer it managed to push first.
        let mut written = 0usize;
        while written < bytes_readed {
            match write.write(&buffer[written..bytes_readed]).await {
                Ok(0) => {
                    return Err(Error::SocketError(
                        "Error writing to socket: writer accepted no bytes".to_string(),
                    ))
                }
                Ok(n) => {
                    written += n;
                    *copied += n;
                }
                Err(err) => {
                    return Err(Error::SocketError(format!(
                        "Error writing to socket: {err:?}"
                    )))
                }
            }
        }

        bytes_remained -= bytes_readed;
        if bytes_remained == 0 {
            break;
        }
        if bytes_remained < buffer_size {
            buffer_size = bytes_remained;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::AsyncWrite;
    use tokio::io::AsyncWriteExt;

    const READ_BUF_DEFAULT_CAPACITY: usize = 8192;

    /// Build a raw PG wire message: [code: u8][len: i32][body...]
    /// len includes itself (4 bytes) but not the code byte.
    fn wire_msg(code: u8, body: &[u8]) -> Vec<u8> {
        let len = (4 + body.len()) as i32;
        let mut msg = Vec::with_capacity(1 + 4 + body.len());
        msg.push(code);
        msg.extend_from_slice(&len.to_be_bytes());
        msg.extend_from_slice(body);
        msg
    }

    #[test]
    fn write_all_half_bounds_admin_protocol_writes() {
        let src = include_str!("socket.rs");
        let start = src
            .find("pub async fn write_all_half")
            .expect("write_all_half helper must exist");
        let end = src[start..]
            .find("/// Read message header.")
            .map(|offset| start + offset)
            .expect("write_all_half block end marker must exist");
        let block = &src[start..end];

        assert!(
            block.contains("timeout(ADMIN_RESPONSE_WRITE_TIMEOUT, stream.write_all(buf))"),
            "admin protocol response writes must be deadline-bound"
        );
        assert!(
            block.contains("timeout(ADMIN_RESPONSE_WRITE_TIMEOUT, stream.flush())"),
            "admin protocol responses must flush under a deadline"
        );
        assert!(
            block.contains("ADMIN_RESPONSE_WRITE_ERRORS_TOTAL"),
            "admin protocol write failures must increment a bounded counter"
        );
        assert!(
            block.contains("admin response"),
            "admin protocol write failures must be logged with admin context"
        );
    }

    struct PendingWrite;

    impl AsyncWrite for PendingWrite {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn write_all_flush_timeout_bounds_stalled_writer() {
        let mut writer = PendingWrite;

        let err = write_all_flush_timeout(&mut writer, b"x", Duration::from_millis(20))
            .await
            .expect_err("stalled writer must time out");

        assert!(matches!(err, Error::ProxyTimeout));
    }

    // =========================================================================
    // read_message_reuse — wire protocol validation
    // =========================================================================

    /// ReadyForQuery 'Z' with body 'I' (idle) — the most common server→client message.
    /// DBA sees this after every successful query. Must parse correctly.
    #[tokio::test]
    async fn reuse_ready_for_query() {
        let data = wire_msg(b'Z', b"I");
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();

        assert_eq!(result.len(), 6); // code(1) + len(4) + body(1)
        assert_eq!(result[0], b'Z');
        assert_eq!(result[5], b'I');
    }

    /// Minimal valid message: len=4 means zero body bytes.
    /// This is a degenerate but valid PG message (e.g., Sync 'S', Flush 'H').
    /// read_exact on a 0-length slice must be a no-op, not an error.
    #[tokio::test]
    async fn reuse_minimal_message_len_4() {
        let data = wire_msg(b'S', &[]); // Sync: code='S', len=4, no body
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();

        assert_eq!(result.len(), 5); // code(1) + len(4) + body(0)
        assert_eq!(result[0], b'S');
        let len = i32::from_be_bytes([result[1], result[2], result[3], result[4]]);
        assert_eq!(len, 4);
    }

    /// CommandComplete 'C' with tag "SELECT 1" — typical query response.
    /// Verifies body bytes are read correctly.
    #[tokio::test]
    async fn reuse_command_complete() {
        let body = b"SELECT 1\0";
        let data = wire_msg(b'C', body);
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();

        assert_eq!(result[0], b'C');
        assert_eq!(&result[5..], body);
    }

    /// len < 4 is a protocol violation — PG length field includes itself (4 bytes).
    /// Pooler must reject without touching memory counter.
    #[tokio::test]
    async fn reuse_len_less_than_4_returns_error() {
        // Craft header manually: code='X', len=3
        let mut data = vec![b'X'];
        data.extend_from_slice(&3_i32.to_be_bytes());
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX).await;

        assert!(result.is_err());
        // Note: CURRENT_MEMORY is a global shared by all tests — don't assert absolute value
    }

    /// Negative length — could happen with corrupted TCP stream or malicious client.
    /// Must be caught by len < 4 check.
    #[tokio::test]
    async fn reuse_negative_len_returns_error() {
        let mut data = vec![b'Q'];
        data.extend_from_slice(&(-1_i32).to_be_bytes());
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX).await;

        assert!(result.is_err());
        // Note: CURRENT_MEMORY is a global shared by all tests — don't assert absolute value
    }

    /// len > MAX_MESSAGE_SIZE (256MB) — prevents OOM from malformed messages.
    #[tokio::test]
    async fn reuse_len_exceeds_max_returns_error() {
        let mut data = vec![b'D'];
        data.extend_from_slice(&(MAX_MESSAGE_SIZE + 1).to_be_bytes());
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX).await;

        assert!(result.is_err());
        // Note: CURRENT_MEMORY is a global shared by all tests - don't assert absolute value
    }

    /// Guard for the fuzz scenario `pg_doorman does not crash on
    /// gigantic message length claim` (tests/bdd/features/fuzz-resilience.feature).
    /// The fuzzer sends `[Q, 0x10, 0x00, 0x00, 0x00]` - a Simple Query frame
    /// header claiming a body length of EXACTLY `MAX_MESSAGE_SIZE` (256MB).
    /// Before the `>=` check, the strict `>` comparison let this value through;
    /// pg_doorman then synchronously allocated 256MB via `buf.resize(...)`
    /// AND held the full memory budget via CURRENT_MEMORY for the duration of
    /// the doomed read_exact, starving every concurrent client of memory and
    /// causing them to bail with `Error::CurrentMemoryUsage`. The valid
    /// client's next message would then see its connection reset by pg_doorman.
    #[tokio::test]
    async fn reuse_len_equals_max_returns_error_no_allocation() {
        let mut data = vec![b'Q'];
        data.extend_from_slice(&MAX_MESSAGE_SIZE.to_be_bytes());
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let before = CURRENT_MEMORY.load(Ordering::SeqCst);
        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX).await;
        let after = CURRENT_MEMORY.load(Ordering::SeqCst);

        assert!(
            result.is_err(),
            "len exactly MAX_MESSAGE_SIZE must be rejected as malformed \
             (no legit protocol traffic sends single message of the defensive maximum)"
        );
        assert_eq!(
            after, before,
            "rejected message must NOT touch CURRENT_MEMORY - the whole point \
             of the early `>= MAX_MESSAGE_SIZE` check is to bail before any \
             allocation or budget accounting"
        );
    }

    /// Companion: same check on `read_message_data` (the lower-level
    /// non-reusable-buffer variant used by `read_message`).
    #[tokio::test]
    async fn read_message_data_len_equals_max_returns_error() {
        let mut stream = Cursor::new(Vec::<u8>::new());
        let result = read_message_data(&mut stream, b'Q', MAX_MESSAGE_SIZE).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_message_data_with_memory_limit_rejects_large_body_before_read() {
        let mut stream = Cursor::new(Vec::<u8>::new());
        let before = CURRENT_MEMORY.load(Ordering::SeqCst);

        let result = read_message_data_with_memory_limit(&mut stream, b'S', 8192, 1).await;

        assert!(
            matches!(result, Err(Error::CurrentMemoryUsage)),
            "oversized startup body must hit the memory budget before reading body bytes: {result:?}"
        );
        assert_eq!(
            CURRENT_MEMORY.load(Ordering::SeqCst),
            before,
            "failed startup body reservation must be rolled back"
        );
    }

    // =========================================================================
    // read_message_reuse — memory pressure
    // =========================================================================

    /// Memory limit exactly hit — message at boundary should be accepted.
    /// DBA sets max_memory to control proxy RAM usage.
    #[tokio::test]
    async fn reuse_memory_limit_high_accepted() {
        let body = vec![0u8; 96]; // len = 100
        let data = wire_msg(b'D', &body);
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        // High limit — always accepted regardless of CURRENT_MEMORY from other tests
        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX).await;
        assert!(result.is_ok());
    }

    /// memory budget only applies to messages at or
    /// above `MEMORY_RESERVATION_SKIP_THRESHOLD` (4 KiB). Smaller
    /// messages bypass the global atomic for cache-line contention
    /// reasons, so a 100-byte frame against `max_memory_usage = 1`
    /// is intentionally accepted now. The budget remains the only
    /// guard against COPY chunks / outsized DataRow that actually
    /// move RSS, which this test still exercises with a large body.
    #[tokio::test]
    async fn reuse_memory_limit_large_rejected() {
        // Body 8 KiB - above the skip threshold so the budget kicks
        // in. With `max_memory_usage = 1` the reservation must fail.
        let body = vec![0u8; 8192];
        let data = wire_msg(b'D', &body);
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, 1).await;
        assert!(result.is_err());
    }

    /// the small-message fast path guard: a sub-threshold message must
    /// succeed even against a near-zero memory budget - that is the
    /// whole point of the bypass and the regression a future change
    /// re-enabling the unconditional reservation would surface here.
    #[tokio::test]
    async fn reuse_small_message_bypasses_budget() {
        let body = vec![0u8; 96]; // len = 100, well under 4 KiB
        let data = wire_msg(b'D', &body);
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_reuse(&mut stream, &mut buf, 1).await;
        assert!(
            result.is_ok(),
            "small messages must bypass CURRENT_MEMORY budget"
        );
    }

    /// Memory counter delta must be 0 after successful read.
    /// Uses delta instead of absolute value to avoid races with parallel tests.
    #[tokio::test]
    async fn reuse_memory_counter_balanced_on_success() {
        let data = wire_msg(b'Z', b"I");
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let before = CURRENT_MEMORY.load(Ordering::SeqCst);
        let _ = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();
        let after = CURRENT_MEMORY.load(Ordering::SeqCst);

        assert_eq!(
            after,
            before,
            "memory counter leaked: delta={}",
            after - before
        );
    }

    /// Memory counter delta must be 0 even on read failure (EOF mid-body).
    /// Uses delta instead of absolute value to avoid races with parallel tests.
    #[tokio::test]
    async fn reuse_memory_counter_balanced_on_read_error() {
        let mut data = vec![b'D'];
        data.extend_from_slice(&100_i32.to_be_bytes());
        let mut stream = Cursor::new(data);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let before = CURRENT_MEMORY.load(Ordering::SeqCst);
        let result = read_message_reuse(&mut stream, &mut buf, u64::MAX).await;
        let after = CURRENT_MEMORY.load(Ordering::SeqCst);

        assert!(result.is_err());
        assert_eq!(
            after,
            before,
            "memory counter leaked on error: delta={}",
            after - before
        );
    }

    // =========================================================================
    // read_message_reuse — buffer management (the core optimization)
    // =========================================================================

    /// Three messages in sequence on the same buffer — capacity should stabilize.
    /// This is the steady-state: after warmup, zero allocations per message.
    #[tokio::test]
    async fn reuse_sequential_messages_stable_capacity() {
        let msg1 = wire_msg(b'Z', b"I");
        let msg2 = wire_msg(b'C', b"SELECT 1\0");
        let msg3 = wire_msg(b'Z', b"T");

        let mut all = Vec::new();
        all.extend_from_slice(&msg1);
        all.extend_from_slice(&msg2);
        all.extend_from_slice(&msg3);

        let mut stream = Cursor::new(all);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let r1 = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();
        let cap_after_first = buf.capacity();

        let r2 = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();
        let cap_after_second = buf.capacity();

        let r3 = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();
        let cap_after_third = buf.capacity();

        // All messages decoded correctly
        assert_eq!(r1[0], b'Z');
        assert_eq!(r2[0], b'C');
        assert_eq!(r3[0], b'Z');

        // Capacity stays within 8KB range (reserve reuses after split)
        assert!(cap_after_first <= READ_BUF_DEFAULT_CAPACITY);
        assert!(cap_after_second <= READ_BUF_DEFAULT_CAPACITY);
        assert!(cap_after_third <= READ_BUF_DEFAULT_CAPACITY);
    }

    /// After a large message, split() hands the big allocation to the caller.
    /// The reusable buf gets near-zero remaining capacity, so the next reserve()
    /// allocates a fresh small buffer. No permanent bloat from a single large message.
    #[tokio::test]
    async fn reuse_large_then_small_no_bloat() {
        let large_body = vec![0u8; 100_000];
        let small_body = vec![0u8; 10];

        let mut all = Vec::new();
        all.extend_from_slice(&wire_msg(b'D', &large_body));
        all.extend_from_slice(&wire_msg(b'Z', &small_body));

        let mut stream = Cursor::new(all);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        // Read large message — reserve() grows, split() takes the data
        let large_msg = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();
        assert_eq!(large_msg.len(), 1 + 4 + 100_000);

        // Read small message — reserve() allocates fresh small buffer
        let small_msg = read_message_reuse(&mut stream, &mut buf, u64::MAX)
            .await
            .unwrap();
        assert_eq!(small_msg[0], b'Z');

        // Buffer capacity is small — no permanent 100KB bloat
        assert!(
            buf.capacity() < 65536,
            "capacity should be small after split pattern: got {}",
            buf.capacity(),
        );
    }

    /// Realistic hot path: the previous message is dropped before the next read.
    /// Each iteration of the client loop reads a `Q`, processes it, and drops the
    /// `BytesMut` before the next call. `BytesMut::split()` lets the reusable
    /// `buf` reclaim the dropped allocation through its Arc, so a 5 MiB capacity
    /// follows the buf into every subsequent small read. Across 15 000 idle
    /// clients that each ran one large INSERT once, this compounds to multi-GiB
    /// pooler RSS. The test fails when the leak is present.
    #[tokio::test]
    async fn reuse_large_dropped_then_small_no_bloat() {
        let large_body = vec![0u8; 5 * 1024 * 1024]; // 5 MiB — matches mcp-ss-bitmaps payloads
        let small_body = vec![0u8; 16];

        let mut all = Vec::new();
        all.extend_from_slice(&wire_msg(b'Q', &large_body));
        for _ in 0..50 {
            all.extend_from_slice(&wire_msg(b'Q', &small_body));
        }

        let mut stream = Cursor::new(all);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        // Read the large Q and immediately drop it — that's what handle_simple_query
        // does once execute_server_roundtrip returns.
        {
            let _ = read_message_reuse(&mut stream, &mut buf, u64::MAX)
                .await
                .unwrap();
        }

        for _ in 0..50 {
            let _msg = read_message_reuse(&mut stream, &mut buf, u64::MAX)
                .await
                .unwrap();
        }

        assert!(
            buf.capacity() <= REUSE_BUF_SHRINK_THRESHOLD,
            "buf bloated after one large read followed by small reads: capacity={} bytes",
            buf.capacity(),
        );
    }

    #[tokio::test]
    async fn cancel_safe_reuse_preserves_partial_header_after_cancel() {
        let msg = wire_msg(b'Q', b"select 1\0");
        let (mut client, mut peer) = tokio::io::duplex(64);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        peer.write_all(&msg[..3]).await.unwrap();
        let mut first_read = Box::pin(read_message_reuse_cancel_safe(
            &mut client,
            &mut buf,
            u64::MAX,
        ));
        tokio::time::timeout(Duration::from_millis(10), &mut first_read)
            .await
            .expect_err("partial header must not complete");
        drop(first_read);

        peer.write_all(&msg[3..]).await.unwrap();
        let decoded = tokio::time::timeout(
            Duration::from_secs(1),
            read_message_reuse_cancel_safe(&mut client, &mut buf, u64::MAX),
        )
        .await
        .expect("second read timed out")
        .expect("second read failed");

        assert_eq!(&decoded[..], msg.as_slice());
    }

    // =========================================================================
    // read_message_body_reuse — server-side path
    // =========================================================================

    /// Standard CommandComplete read when header is already consumed by recv().
    #[tokio::test]
    async fn body_reuse_standard_message() {
        let body = b"SELECT 1\0";
        let len = (4 + body.len()) as i32;

        // Stream contains ONLY body (header already consumed)
        let mut stream = Cursor::new(body.to_vec());
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_body_reuse(&mut stream, &mut buf, b'C', len)
            .await
            .unwrap();

        assert_eq!(result[0], b'C');
        let result_len = i32::from_be_bytes([result[1], result[2], result[3], result[4]]);
        assert_eq!(result_len, len);
        assert_eq!(&result[5..], body);
    }

    /// Minimal body: len=4, zero body bytes. Header takes 5 bytes, body is empty.
    #[tokio::test]
    async fn body_reuse_minimal_len_4() {
        let mut stream = Cursor::new(Vec::<u8>::new()); // no body to read
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_body_reuse(&mut stream, &mut buf, b'H', 4)
            .await
            .unwrap();

        assert_eq!(result.len(), 5); // code(1) + len(4)
        assert_eq!(result[0], b'H');
    }

    /// len < 4 is a protocol violation — must return error, not panic.
    #[tokio::test]
    async fn body_reuse_len_less_than_4_returns_error() {
        let mut stream = Cursor::new(Vec::<u8>::new());
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_body_reuse(&mut stream, &mut buf, b'D', 3).await;
        assert!(result.is_err());
    }

    /// Negative length from corrupted TCP stream — must return error, not panic.
    #[tokio::test]
    async fn body_reuse_negative_len_returns_error() {
        let mut stream = Cursor::new(Vec::<u8>::new());
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_body_reuse(&mut stream, &mut buf, b'D', -1).await;
        assert!(result.is_err());
    }

    /// EOF during body read — simulates TCP connection reset from PostgreSQL.
    /// DBA sees this when PG crashes or network partition during query.
    #[tokio::test]
    async fn body_reuse_eof_mid_body() {
        // Claim 1000 bytes body but provide only 10
        let partial_body = vec![0u8; 10];
        let mut stream = Cursor::new(partial_body);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_body_reuse(&mut stream, &mut buf, b'D', 1004).await;

        assert!(result.is_err());
    }

    /// split() returns data independent from the reusable buffer.
    /// Critical: mutation of returned bytes must not affect buf, and vice versa.
    #[tokio::test]
    async fn body_reuse_split_returns_independent_data() {
        let body = b"test_data\0";
        let len = (4 + body.len()) as i32;
        let mut stream = Cursor::new(body.to_vec());
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let result = read_message_body_reuse(&mut stream, &mut buf, b'C', len)
            .await
            .unwrap();

        // buf should be empty after split
        assert_eq!(buf.len(), 0);
        // result should have the data
        assert_eq!(result[0], b'C');
        assert_eq!(&result[5..], body);
    }

    /// Server-side path of the same regression. `Server.read_buf` must not
    /// retain a multi-MiB allocation after one large `DataRow` or `CopyData`
    /// passes through. Long-lived backend connections in transaction mode
    /// would otherwise pin RSS to the size of the largest message handled.
    #[tokio::test]
    async fn body_reuse_large_dropped_then_small_no_bloat() {
        let large_body = vec![0u8; 5 * 1024 * 1024];
        let large_len = (4 + large_body.len()) as i32;
        let small_body = vec![0u8; 16];
        let small_len = (4 + small_body.len()) as i32;

        let mut all = Vec::new();
        all.extend_from_slice(&large_body);
        for _ in 0..50 {
            all.extend_from_slice(&small_body);
        }

        let mut stream = Cursor::new(all);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        {
            let _ = read_message_body_reuse(&mut stream, &mut buf, b'D', large_len)
                .await
                .unwrap();
        }

        for _ in 0..50 {
            let _ = read_message_body_reuse(&mut stream, &mut buf, b'D', small_len)
                .await
                .unwrap();
        }

        assert!(
            buf.capacity() <= REUSE_BUF_SHRINK_THRESHOLD,
            "buf bloated after one large body read followed by small reads: capacity={} bytes",
            buf.capacity(),
        );
    }

    /// After split, buf retains capacity for next message (the optimization).
    #[tokio::test]
    async fn body_reuse_capacity_preserved_after_split() {
        let body = vec![0u8; 4000];
        let len = (4 + body.len()) as i32;
        let mut stream = Cursor::new(body);
        let mut buf = BytesMut::with_capacity(READ_BUF_DEFAULT_CAPACITY);

        let _ = read_message_body_reuse(&mut stream, &mut buf, b'D', len)
            .await
            .unwrap();

        // buf.len() == 0 after split, but capacity should allow next message
        assert_eq!(buf.len(), 0);
        // For read_buf (single-message pattern), split leaves remainder capacity.
        // Next reserve() will reuse or grow as needed. This is correct behavior.
    }

    #[test]
    fn reusable_readers_must_not_set_len_over_uninitialized_capacity() {
        let source = include_str!("socket.rs");
        let forbidden = concat!("unsafe { buf.", "set_len(total_len); }");
        assert!(
            !source.contains(forbidden),
            "reusable readers must not form &mut [u8] over uninitialized BytesMut capacity"
        );
    }

    // =========================================================================
    // clone() vs split() for accumulation buffers — the design decision
    // =========================================================================

    /// Documents WHY server.buffer uses clone()+clear() instead of split().
    /// split() on a full buffer leaves near-zero remaining capacity,
    /// forcing reallocation on the next put_slice(). clone()+clear() preserves
    /// the warm capacity.
    #[tokio::test]
    async fn clone_clear_preserves_capacity_for_accumulation() {
        let mut buffer = BytesMut::with_capacity(8192);
        buffer.put_slice(&[0u8; 6000]); // accumulate like recv() does

        let cap_before = buffer.capacity();
        let _bytes = buffer.clone();
        buffer.clear();
        let cap_after = buffer.capacity();

        assert_eq!(
            cap_before, cap_after,
            "clone()+clear() must preserve capacity for accumulation buffers"
        );
    }

    /// Demonstrates that split() does NOT preserve capacity — the reason we
    /// reverted to clone()+clear() for server.buffer.
    #[tokio::test]
    async fn split_does_not_preserve_capacity() {
        let mut buffer = BytesMut::with_capacity(8192);
        buffer.put_slice(&[0u8; 6000]);

        let cap_before = buffer.capacity();
        let _bytes = buffer.split();
        let cap_after = buffer.capacity();

        assert!(cap_after < cap_before,
            "split() leaves remainder capacity ({cap_after}) much less than original ({cap_before})");
    }

    /// `proxy_copy_data` must report the bytes that actually reached the
    /// writer when the writer fails partway through, not the full
    /// declared frame size. The streaming-byte counter relies on this
    /// to avoid overstating large-message throughput on disconnects.
    #[tokio::test]
    async fn proxy_copy_data_partial_writer_failure_records_actual_bytes() {
        use std::io::ErrorKind;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tokio::io::AsyncWrite;

        struct LimitedWriter {
            limit: usize,
            written: usize,
        }

        impl AsyncWrite for LimitedWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                let me = self.get_mut();
                if me.written >= me.limit {
                    return Poll::Ready(Err(std::io::Error::new(
                        ErrorKind::BrokenPipe,
                        "test writer hit its limit",
                    )));
                }
                let take = buf.len().min(me.limit - me.written);
                me.written += take;
                Poll::Ready(Ok(take))
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        // Source has 1000 bytes; writer truncates after 200.
        let payload = vec![0xABu8; 1000];
        let mut reader = Cursor::new(payload);
        let mut writer = LimitedWriter {
            limit: 200,
            written: 0,
        };

        let mut copied: usize = 0;
        let res = proxy_copy_data(&mut reader, &mut writer, 1000, &mut copied).await;

        assert!(
            res.is_err(),
            "writer failure must propagate as `Err`, got Ok"
        );
        assert!(
            copied <= 200,
            "must not over-report bytes that never made it past the writer (copied = {copied})",
        );
        assert!(
            copied > 0,
            "writer accepted some bytes before failing — counter must reflect them",
        );
        assert!(
            copied < 1000,
            "the failure must abort short of the declared frame size",
        );
    }
}
