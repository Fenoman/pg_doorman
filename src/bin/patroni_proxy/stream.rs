use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

/// Proxy result
#[derive(Debug)]
pub struct ProxyResult {
    /// Number of bytes transferred from client to server
    pub client_to_server_bytes: u64,
    /// Number of bytes transferred from server to client
    pub server_to_client_bytes: u64,
}

/// TCP proxy errors
#[derive(Debug)]
pub enum ProxyError {
    /// Server connection error
    ConnectionFailed(io::Error),
    /// I/O error during proxying
    IoError(io::Error),
    /// Connection was stopped via stop()
    Stopped,
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::ConnectionFailed(e) => write!(f, "Connection failed: {e}"),
            ProxyError::IoError(e) => write!(f, "IO error: {e}"),
            ProxyError::Stopped => write!(f, "Proxy stopped"),
        }
    }
}

impl std::error::Error for ProxyError {}

impl From<io::Error> for ProxyError {
    fn from(e: io::Error) -> Self {
        ProxyError::IoError(e)
    }
}

/// High-performance TCP proxy for proxying connections
/// between client and server (host:port).
///
/// Uses tokio for asynchronous I/O and supports
/// graceful shutdown via stop() method.
pub struct TcpProxy {
    /// Server address to connect to
    server_addr: SocketAddr,
    /// Timeout for establishing the backend TCP connection. Bounds how long
    /// a black-holed backend can stall the client before we give up.
    connect_timeout: std::time::Duration,
    /// Stop flag
    stopped: Arc<AtomicBool>,
    /// Stop notification
    stop_notify: Arc<Notify>,
}

/// Default backend connect timeout. Without a bound, a black-holed backend
/// would hang the client for the OS default connect timeout (60-120s).
const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl TcpProxy {
    /// Creates a new TCP proxy for the specified server address.
    ///
    /// # Arguments
    /// * `server_addr` - Server address (ip:port) to connect to
    ///
    /// # Example
    /// ```ignore
    /// let proxy = TcpProxy::new("127.0.0.1:5432".parse().unwrap());
    /// ```
    pub fn new(server_addr: SocketAddr) -> Self {
        Self {
            server_addr,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            stopped: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
        }
    }

    /// Overrides the backend connect timeout (builder style).
    #[cfg(test)]
    pub fn with_connect_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Returns a handle for stopping the proxy.
    ///
    /// The handle can be cloned and used from another thread/task
    /// to stop proxying.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            stopped: Arc::clone(&self.stopped),
            stop_notify: Arc::clone(&self.stop_notify),
        }
    }

    /// Establishes a TCP connection to the server and starts proxying
    /// data between client and server.
    ///
    /// Uses high-performance bidirectional copying
    /// with tokio::io::copy_bidirectional.
    ///
    /// # Arguments
    /// * `client_stream` - Client TCP connection
    ///
    /// # Returns
    /// * `Ok(ProxyResult)` - Proxy statistics on normal completion
    /// * `Err(ProxyError)` - Error during connection or proxying
    ///
    /// # Example
    /// ```ignore
    /// let proxy = TcpProxy::new("127.0.0.1:5432".parse().unwrap());
    /// let result = proxy.run(client_stream).await?;
    /// println!("Transferred {} bytes to server", result.client_to_server_bytes);
    /// ```
    pub async fn run(self, client_stream: TcpStream) -> Result<ProxyResult, ProxyError> {
        // Check if proxy was stopped before starting
        if self.stopped.load(Ordering::SeqCst) {
            return Err(ProxyError::Stopped);
        }

        // Connect to server, bounded by connect_timeout so a black-holed
        // backend cannot hang the client for the OS default (60-120s).
        let server_stream =
            match tokio::time::timeout(self.connect_timeout, TcpStream::connect(self.server_addr))
                .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => return Err(ProxyError::ConnectionFailed(e)),
                Err(_) => {
                    return Err(ProxyError::ConnectionFailed(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "connect to backend {} timed out after {:?}",
                            self.server_addr, self.connect_timeout
                        ),
                    )))
                }
            };

        // Set TCP_NODELAY for minimal latency
        let _ = client_stream.set_nodelay(true);
        let _ = server_stream.set_nodelay(true);

        // Start proxying
        self.proxy_streams(client_stream, server_stream).await
    }

    /// Proxies data between two streams with stop support.
    async fn proxy_streams(
        self,
        client_stream: TcpStream,
        server_stream: TcpStream,
    ) -> Result<ProxyResult, ProxyError> {
        let (client_read, client_write) = client_stream.into_split();
        let (server_read, server_write) = server_stream.into_split();

        let stopped = Arc::clone(&self.stopped);
        let stop_notify = Arc::clone(&self.stop_notify);

        // Create tasks for copying in both directions
        let client_to_server = copy_with_stop(
            client_read,
            server_write,
            Arc::clone(&stopped),
            Arc::clone(&stop_notify),
        );

        let server_to_client = copy_with_stop(server_read, client_write, stopped, stop_notify);

        // Wait for both tasks to complete
        let (c2s_result, s2c_result) = tokio::join!(client_to_server, server_to_client);

        // Check results
        let client_to_server_bytes = c2s_result?;
        let server_to_client_bytes = s2c_result?;

        Ok(ProxyResult {
            client_to_server_bytes,
            server_to_client_bytes,
        })
    }
}

/// Handle for stopping TCP proxy.
///
/// Can be cloned and used from any thread/task.
#[derive(Clone)]
pub struct StopHandle {
    stopped: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
}

impl StopHandle {
    /// Stops proxying.
    ///
    /// After calling this method, all active proxying operations
    /// will be interrupted and connections will be closed.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop_notify.notify_waiters();
    }

    /// Checks if the proxy was stopped.
    #[cfg(test)]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Checks whether two handles refer to the same proxy task.
    pub fn is_same_proxy(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.stopped, &other.stopped)
    }
}

/// Copies data from reader to writer with stop support.
async fn copy_with_stop<R, W>(
    mut reader: R,
    mut writer: W,
    stopped: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
) -> Result<u64, ProxyError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut total_bytes: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024]; // 64KB buffer for high performance

    loop {
        let stop_notified = stop_notify.notified();
        tokio::pin!(stop_notified);
        stop_notified.as_mut().enable();

        // Check stop flag
        if stopped.load(Ordering::SeqCst) {
            return Err(ProxyError::Stopped);
        }

        tokio::select! {
            // Wait for stop notification
            _ = &mut stop_notified => {
                return Err(ProxyError::Stopped);
            }
            // Read data
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        // EOF - connection closed
                        let stop_notified = stop_notify.notified();
                        tokio::pin!(stop_notified);
                        stop_notified.as_mut().enable();
                        if stopped.load(Ordering::SeqCst) {
                            return Err(ProxyError::Stopped);
                        }
                        tokio::select! {
                            _ = &mut stop_notified => {
                                return Err(ProxyError::Stopped);
                            }
                            result = writer.shutdown() => {
                                let _ = result;
                            }
                        }
                        return Ok(total_bytes);
                    }
                    Ok(n) => {
                        // Write data
                        let stop_notified = stop_notify.notified();
                        tokio::pin!(stop_notified);
                        stop_notified.as_mut().enable();
                        if stopped.load(Ordering::SeqCst) {
                            return Err(ProxyError::Stopped);
                        }
                        tokio::select! {
                            _ = &mut stop_notified => {
                                return Err(ProxyError::Stopped);
                            }
                            result = writer.write_all(&buf[..n]) => {
                                if let Err(e) = result {
                                    return Err(ProxyError::IoError(e));
                                }
                            }
                        }
                        total_bytes += n as u64;
                    }
                    Err(e) => {
                        return Err(ProxyError::IoError(e));
                    }
                }
            }
        }
    }
}

/// Creates a TCP proxy and starts proxying in a separate task.
///
/// Returns a handle for stopping the proxy.
///
/// # Arguments
/// * `server_addr` - Server address to connect to
/// * `client_stream` - Client TCP connection
///
/// # Returns
/// * `StopHandle` - Handle for stopping the proxy
/// * `JoinHandle` - Handle for waiting for task completion
///
/// # Example
/// ```ignore
/// let (stop_handle, join_handle) = spawn_proxy(
///     "127.0.0.1:5432".parse().unwrap(),
///     client_stream,
/// );
///
/// // Later, to stop:
/// stop_handle.stop();
/// let result = join_handle.await;
/// ```
pub fn spawn_proxy(
    server_addr: SocketAddr,
    client_stream: TcpStream,
) -> (
    StopHandle,
    tokio::task::JoinHandle<Result<ProxyResult, ProxyError>>,
) {
    let proxy = TcpProxy::new(server_addr);
    let stop_handle = proxy.stop_handle();

    let join_handle = tokio::spawn(async move { proxy.run(client_stream).await });

    (stop_handle, join_handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_tcp_proxy_basic() {
        // Create test server
        let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_listener.local_addr().unwrap();

        // Create test "client" (actually this will be our incoming stream)
        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();

        // Start server that responds to data
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = server_listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            // Send data back with prefix
            let response = format!("ECHO: {}", String::from_utf8_lossy(&buf[..n]));
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        // Connect to client listener
        let client_connect_task = tokio::spawn(async move {
            let mut stream = TcpStream::connect(client_addr).await.unwrap();
            stream.write_all(b"Hello, World!").await.unwrap();
            stream.shutdown().await.unwrap();

            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            response
        });

        // Accept connection from "client" and proxy to server
        let (client_stream, _) = client_listener.accept().await.unwrap();
        let proxy = TcpProxy::new(server_addr);
        let result = proxy.run(client_stream).await.unwrap();

        // Check results
        assert!(result.client_to_server_bytes > 0);
        assert!(result.server_to_client_bytes > 0);

        // Wait for tasks to complete
        server_task.await.unwrap();
        let response = client_connect_task.await.unwrap();
        assert!(String::from_utf8_lossy(&response).contains("ECHO:"));
    }

    #[tokio::test]
    async fn test_tcp_proxy_stop() {
        // Create test server that keeps connection open
        let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (mut stream, _) = server_listener.accept().await.unwrap();
            // Keep connection open
            tokio::time::sleep(Duration::from_secs(10)).await;
            let _ = stream.shutdown().await;
        });

        // Create client connection
        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();

        let client_task = tokio::spawn(async move {
            let stream = TcpStream::connect(client_addr).await.unwrap();
            // Keep connection open
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(stream);
        });

        // Accept connection and start proxy
        let (client_stream, _) = client_listener.accept().await.unwrap();
        let (stop_handle, join_handle) = spawn_proxy(server_addr, client_stream);

        // Give proxy some time to establish connection
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Stop proxy
        stop_handle.stop();

        // Check that proxy stopped
        let result = tokio::time::timeout(Duration::from_secs(1), join_handle).await;
        assert!(result.is_ok(), "Proxy should stop within timeout");

        let proxy_result = result.unwrap().unwrap();
        assert!(matches!(proxy_result, Err(ProxyError::Stopped)));

        // Cancel background tasks
        server_task.abort();
        client_task.abort();
    }

    #[tokio::test]
    async fn test_stop_handle_clone() {
        let proxy = TcpProxy::new("127.0.0.1:5432".parse().unwrap());
        let handle1 = proxy.stop_handle();
        let handle2 = handle1.clone();

        assert!(!handle1.is_stopped());
        assert!(!handle2.is_stopped());

        handle1.stop();

        assert!(handle1.is_stopped());
        assert!(handle2.is_stopped());
    }

    #[test]
    fn copy_with_stop_enables_stop_notification_before_flag_check() {
        let src = include_str!("stream.rs");
        let start = src
            .find("async fn copy_with_stop")
            .expect("copy_with_stop must exist");
        let end = src[start..]
            .find("/// Creates a TCP proxy")
            .map(|offset| start + offset)
            .expect("copy_with_stop end marker must exist");
        let body = &src[start..end];

        let enable = body
            .find(".enable()")
            .expect("copy_with_stop must enable Notified before checking stopped");
        let stopped_check = body
            .find("stopped.load")
            .expect("copy_with_stop must check the stopped flag");

        assert!(
            enable < stopped_check,
            "stop notification must be registered before the stopped flag check \
             to avoid losing stop() between the check and select registration"
        );
    }

    #[tokio::test]
    async fn copy_with_stop_interrupts_pending_write() {
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct OneRead {
            sent: bool,
        }

        impl AsyncRead for OneRead {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                if self.sent {
                    return Poll::Pending;
                }
                self.sent = true;
                buf.put_slice(b"ping");
                Poll::Ready(Ok(()))
            }
        }

        struct PendingWrite {
            entered_write: Arc<Notify>,
        }

        impl AsyncWrite for PendingWrite {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                self.entered_write.notify_waiters();
                Poll::Pending
            }

            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let stopped = Arc::new(AtomicBool::new(false));
        let stop_notify = Arc::new(Notify::new());
        let entered_write = Arc::new(Notify::new());

        let copy_task = tokio::spawn(copy_with_stop(
            OneRead { sent: false },
            PendingWrite {
                entered_write: Arc::clone(&entered_write),
            },
            Arc::clone(&stopped),
            Arc::clone(&stop_notify),
        ));

        tokio::time::timeout(Duration::from_millis(100), entered_write.notified())
            .await
            .expect("copy task must reach the pending write");

        stopped.store(true, Ordering::SeqCst);
        stop_notify.notify_waiters();

        let result = tokio::time::timeout(Duration::from_millis(100), copy_task)
            .await
            .expect("stop must interrupt a pending write")
            .expect("copy task must not panic");

        assert!(matches!(result, Err(ProxyError::Stopped)));
    }

    #[tokio::test]
    async fn test_proxy_connection_failed() {
        // Try to connect to non-existent server
        let server_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();

        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();

        let client_task =
            tokio::spawn(async move { TcpStream::connect(client_addr).await.unwrap() });

        let (client_stream, _) = client_listener.accept().await.unwrap();
        let proxy = TcpProxy::new(server_addr);
        let result = proxy.run(client_stream).await;

        assert!(matches!(result, Err(ProxyError::ConnectionFailed(_))));

        client_task.abort();
    }

    // a black-holed backend must NOT hang the client for the OS
    // default connect timeout (60-120s). TcpProxy::run must apply its
    // connect_timeout and fail fast. 192.0.2.1 (TEST-NET-1, RFC 5737) is
    // unrouted and silently drops SYNs, so without a timeout this would hang.
    #[tokio::test]
    async fn run_times_out_on_blackholed_backend() {
        let server_addr: SocketAddr = "192.0.2.1:5432".parse().unwrap();

        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();
        let client_task =
            tokio::spawn(async move { TcpStream::connect(client_addr).await.unwrap() });
        let (client_stream, _) = client_listener.accept().await.unwrap();

        let proxy = TcpProxy::new(server_addr).with_connect_timeout(Duration::from_millis(200));

        // Outer guard: if the inner timeout is missing this would block ~120s.
        let outcome = tokio::time::timeout(Duration::from_secs(3), proxy.run(client_stream)).await;

        assert!(
            outcome.is_ok(),
            "run() did not return within 3s -> connect timeout not applied"
        );
        match outcome.unwrap() {
            Err(ProxyError::ConnectionFailed(e)) => {
                assert_eq!(
                    e.kind(),
                    io::ErrorKind::TimedOut,
                    "blackhole connect must surface as a TimedOut ConnectionFailed"
                );
            }
            other => panic!("expected ConnectionFailed(TimedOut), got {other:?}"),
        }

        client_task.abort();
    }

    #[tokio::test]
    async fn test_proxy_already_stopped() {
        let proxy = TcpProxy::new("127.0.0.1:5432".parse().unwrap());
        let stop_handle = proxy.stop_handle();

        // Stop before starting
        stop_handle.stop();

        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();

        let client_task =
            tokio::spawn(async move { TcpStream::connect(client_addr).await.unwrap() });

        let (client_stream, _) = client_listener.accept().await.unwrap();
        let result = proxy.run(client_stream).await;

        assert!(matches!(result, Err(ProxyError::Stopped)));

        client_task.abort();
    }
}
