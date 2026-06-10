use crate::cluster_manager::ClusterManager;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, error, info, warn};

/// rate-limit `/update_clusters` to at most one
/// invocation per `MIN_INTERVAL_SECS` seconds. Without this gate, any
/// reachable client could amplify a single HTTP request into N×M
/// outbound calls to Patroni REST nodes (one per cluster × host), DoS-
/// ing the operator's etcd-backed Patroni cluster. State-changing
/// refreshes require POST, and a configured `PATRONI_PROXY_ADMIN_TOKEN`
/// is mandatory for every origin, including loopback.
const UPDATE_CLUSTERS_MIN_INTERVAL_SECS: u64 = 5;
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HTTP_CONNECTIONS: usize = 256;
static UPDATE_CLUSTERS_LAST_AT: AtomicU64 = AtomicU64::new(0);
static UPDATE_CLUSTERS_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

struct UpdateClustersInProgressGuard;

impl Drop for UpdateClustersInProgressGuard {
    fn drop(&mut self) {
        UPDATE_CLUSTERS_IN_PROGRESS.store(false, Ordering::Release);
    }
}

async fn read_http_request_with_timeout<S>(
    stream: &mut S,
    buffer: &mut [u8],
    timeout: Duration,
) -> io::Result<Option<usize>>
where
    S: AsyncRead + Unpin,
{
    match tokio::time::timeout(timeout, stream.read(buffer)).await {
        Ok(Ok(0)) => Ok(None),
        Ok(Ok(n)) => Ok(Some(n)),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out reading HTTP request",
        )),
    }
}

async fn write_all_with_timeout<S>(stream: &mut S, bytes: &[u8]) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_all_with_deadline(stream, bytes, HTTP_WRITE_TIMEOUT).await
}

async fn write_all_with_deadline<S>(
    stream: &mut S,
    bytes: &[u8],
    timeout: Duration,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match tokio::time::timeout(timeout, stream.write_all(bytes)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out writing HTTP response",
        )),
    }
}

fn is_loopback(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn token_header_value(request: &str) -> Option<&str> {
    for line in request.lines().skip(1) {
        if let Some(rest) = line
            .strip_prefix("X-Patroni-Proxy-Token: ")
            .or_else(|| line.strip_prefix("x-patroni-proxy-token: "))
        {
            return Some(rest.trim());
        }
    }
    None
}

fn unauthorized_response(reason: &'static str) -> String {
    let body = format!("{reason}\n");
    format!(
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn method_not_allowed_response(allowed: &'static str) -> String {
    let body = format!("method not allowed; use {allowed}\n");
    format!(
        "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: text/plain\r\nAllow: {}\r\nContent-Length: {}\r\n\r\n{}",
        allowed,
        body.len(),
        body
    )
}

fn rate_limited_response(retry_after_secs: u64) -> String {
    let body = format!("rate limited; retry in {retry_after_secs}s\n");
    format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: text/plain\r\nRetry-After: {}\r\nContent-Length: {}\r\n\r\n{}",
        retry_after_secs,
        body.len(),
        body
    )
}

fn update_clusters_in_progress_response() -> String {
    let body = "update already in progress\n";
    format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: text/plain\r\nRetry-After: 1\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn try_acquire_update_clusters_slot(now: u64) -> Result<(), u64> {
    loop {
        let last = UPDATE_CLUSTERS_LAST_AT.load(Ordering::Relaxed);
        let elapsed = now.saturating_sub(last);
        if elapsed < UPDATE_CLUSTERS_MIN_INTERVAL_SECS {
            return Err(UPDATE_CLUSTERS_MIN_INTERVAL_SECS - elapsed);
        }
        if UPDATE_CLUSTERS_LAST_AT
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(());
        }
    }
}

fn try_acquire_update_clusters_in_progress() -> Option<UpdateClustersInProgressGuard> {
    UPDATE_CLUSTERS_IN_PROGRESS
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .ok()
        .map(|_| UpdateClustersInProgressGuard)
}

#[derive(Debug, PartialEq, Eq)]
enum UpdateClustersAccess {
    Allowed,
    Unauthorized,
    MethodNotAllowed,
}

fn authorize_update_clusters_request(
    method: &str,
    request: &str,
    client_addr: &SocketAddr,
    token_env: Option<&str>,
) -> UpdateClustersAccess {
    if method != "POST" {
        return UpdateClustersAccess::MethodNotAllowed;
    }

    match token_env.filter(|token| !token.is_empty()) {
        Some(token) => {
            if token_header_value(request) == Some(token) {
                UpdateClustersAccess::Allowed
            } else {
                UpdateClustersAccess::Unauthorized
            }
        }
        None if is_loopback(client_addr) => UpdateClustersAccess::Allowed,
        None => UpdateClustersAccess::Unauthorized,
    }
}

/// Start minimal HTTP server for health checks and metrics
pub async fn start_http_server(
    listen_addr: String,
    cluster_managers: Arc<RwLock<HashMap<String, Arc<ClusterManager>>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = listen_addr.parse()?;
    let listener = TcpListener::bind(addr).await?;

    info!("HTTP server listening on {}", addr);

    tokio::spawn(async move {
        let connection_slots = Arc::new(Semaphore::new(MAX_HTTP_CONNECTIONS));
        loop {
            match listener.accept().await {
                Ok((mut stream, client_addr)) => {
                    let Ok(connection_permit) = Arc::clone(&connection_slots).try_acquire_owned()
                    else {
                        warn!(
                            "[patroni-proxy] dropping HTTP connection from {client_addr}: \
                             connection limit reached"
                        );
                        continue;
                    };
                    let managers = Arc::clone(&cluster_managers);
                    tokio::spawn(async move {
                        let _connection_permit = connection_permit;
                        let mut buffer = vec![0u8; 4096];

                        match read_http_request_with_timeout(
                            &mut stream,
                            &mut buffer,
                            HTTP_READ_TIMEOUT,
                        )
                        .await
                        {
                            Ok(Some(n)) => {
                                let request = String::from_utf8_lossy(&buffer[..n]);
                                let first_line = request.lines().next().unwrap_or("");
                                debug!("HTTP request from {}: {}", client_addr, first_line);

                                // Parse request method/path
                                let mut first_line_parts = first_line.split_whitespace();
                                let method = first_line_parts.next().unwrap_or("");
                                let path = first_line_parts.next().unwrap_or("/");

                                let response = match path {
                                    "/update_clusters" => {
                                        let token_env =
                                            std::env::var("PATRONI_PROXY_ADMIN_TOKEN").ok();
                                        match authorize_update_clusters_request(
                                            method,
                                            &request,
                                            &client_addr,
                                            token_env.as_deref(),
                                        ) {
                                            UpdateClustersAccess::MethodNotAllowed => {
                                                method_not_allowed_response("POST")
                                            }
                                            UpdateClustersAccess::Unauthorized => {
                                                warn!(
                                                    "[patroni-proxy] /update_clusters denied for {} without valid token",
                                                    client_addr
                                                );
                                                unauthorized_response(
                                                    "valid X-Patroni-Proxy-Token required",
                                                )
                                            }
                                            UpdateClustersAccess::Allowed => {
                                                // Rate limit (cluster-fanout amplifier).
                                                let now = unix_now_secs();
                                                match try_acquire_update_clusters_slot(now) {
                                                    Ok(()) => handle_update_clusters(managers).await,
                                                    Err(retry) => rate_limited_response(retry),
                                                }
                                            }
                                        }
                                    }
                                    _ => {
                                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nOK".to_string()
                                    }
                                };

                                if let Err(e) =
                                    write_all_with_timeout(&mut stream, response.as_bytes()).await
                                {
                                    debug!("Failed to send response to {}: {}", client_addr, e);
                                }
                            }
                            Ok(None) => {
                                debug!("Empty request from {}", client_addr);
                            }
                            Err(e) => {
                                debug!("Failed to read request from {}: {}", client_addr, e);
                            }
                        }

                        let _ = stream.shutdown().await;
                    });
                }
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                }
            }
        }
    });

    Ok(())
}

/// Handle /update_clusters endpoint - trigger immediate update of all clusters
async fn handle_update_clusters(
    cluster_managers: Arc<RwLock<HashMap<String, Arc<ClusterManager>>>>,
) -> String {
    info!("Received request to update all clusters");

    let Some(_in_progress_guard) = try_acquire_update_clusters_in_progress() else {
        warn!("Rejected /update_clusters request because another update is in progress");
        return update_clusters_in_progress_response();
    };

    let managers = cluster_managers.read().await;
    let managers_snapshot: Vec<(String, Arc<ClusterManager>)> = managers
        .iter()
        .map(|(cluster_name, manager)| (cluster_name.clone(), Arc::clone(manager)))
        .collect();
    drop(managers);

    let mut updated_count = 0;

    for (cluster_name, manager) in managers_snapshot {
        info!("Updating cluster '{}'", cluster_name);
        manager.update_members().await;
        updated_count += 1;
    }

    let message = format!("Updated {updated_count} cluster(s)");
    info!("{}", message);

    let response_body = format!("{message}\n");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        response_body.len(),
        response_body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    struct PendingWriter;

    impl AsyncWrite for PendingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn idle_http_request_read_times_out() {
        let (_client, mut server) = tokio::io::duplex(64);
        let mut buffer = [0u8; 16];

        let result =
            read_http_request_with_timeout(&mut server, &mut buffer, Duration::from_millis(20))
                .await;

        assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn http_response_write_times_out() {
        let mut writer = PendingWriter;

        let result =
            write_all_with_deadline(&mut writer, b"response", Duration::from_millis(20)).await;

        assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
    }

    #[test]
    fn update_clusters_slot_allows_only_one_request_per_window() {
        UPDATE_CLUSTERS_LAST_AT.store(100, Ordering::Relaxed);

        assert_eq!(try_acquire_update_clusters_slot(106), Ok(()));
        assert_eq!(try_acquire_update_clusters_slot(106), Err(5));
    }

    #[test]
    fn update_clusters_in_progress_guard_is_single_flight() {
        UPDATE_CLUSTERS_IN_PROGRESS.store(false, Ordering::Relaxed);

        let guard = try_acquire_update_clusters_in_progress()
            .expect("first update_clusters request should enter");
        assert!(
            try_acquire_update_clusters_in_progress().is_none(),
            "concurrent update_clusters request must be rejected while one is in progress"
        );
        drop(guard);
        assert!(
            try_acquire_update_clusters_in_progress().is_some(),
            "single-flight guard must release after the update finishes"
        );
        UPDATE_CLUSTERS_IN_PROGRESS.store(false, Ordering::Relaxed);
    }

    #[test]
    fn update_clusters_requires_post() {
        let client_addr: SocketAddr = "127.0.0.1:45678".parse().unwrap();

        assert_eq!(
            authorize_update_clusters_request(
                "GET",
                "GET /update_clusters HTTP/1.1\r\nHost: localhost\r\n\r\n",
                &client_addr,
                None,
            ),
            UpdateClustersAccess::MethodNotAllowed
        );
    }

    #[test]
    fn configured_update_clusters_token_is_required_on_loopback() {
        let client_addr: SocketAddr = "127.0.0.1:45678".parse().unwrap();

        assert_eq!(
            authorize_update_clusters_request(
                "POST",
                "POST /update_clusters HTTP/1.1\r\nHost: localhost\r\n\r\n",
                &client_addr,
                Some("secret"),
            ),
            UpdateClustersAccess::Unauthorized
        );
        assert_eq!(
            authorize_update_clusters_request(
                "POST",
                "POST /update_clusters HTTP/1.1\r\nHost: localhost\r\nX-Patroni-Proxy-Token: secret\r\n\r\n",
                &client_addr,
                Some("secret"),
            ),
            UpdateClustersAccess::Allowed
        );
    }

    #[test]
    fn update_clusters_drops_manager_map_lock_before_network_refresh() {
        let src = include_str!("api.rs");
        let start = src
            .find("async fn handle_update_clusters")
            .expect("handle_update_clusters should exist");
        let body = &src[start..];
        let end = body.find("\n#[cfg(test)]").unwrap_or(body.len());
        let body = &body[..end];

        assert!(
            body.contains("let managers_snapshot"),
            "update_clusters must snapshot Arc<ClusterManager> values under the read lock"
        );
        assert!(
            body.contains("drop(managers);"),
            "update_clusters must drop the cluster_managers read lock before network awaits"
        );
        let drop_idx = body
            .find("drop(managers);")
            .expect("drop(managers) should exist");
        let update_idx = body
            .find("manager.update_members().await")
            .expect("update_members await should exist");
        assert!(
            drop_idx < update_idx,
            "update_clusters must not hold cluster_managers.read() across update_members().await"
        );
    }

    #[test]
    fn http_response_writes_are_deadline_bound() {
        let src = include_str!("api.rs");
        let start = src
            .find("pub async fn start_http_server")
            .expect("start_http_server should exist");
        let body = &src[start..];
        let end = body
            .find("\n/// Handle /update_clusters endpoint")
            .expect("handler should follow HTTP server");
        let body = &body[..end];

        assert!(
            body.contains("write_all_with_timeout(&mut stream, response.as_bytes())"),
            "patroni_proxy HTTP responses must use a bounded write helper"
        );
    }
}
