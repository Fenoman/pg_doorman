//! HTTP/1.1 keep-alive connection driver. Each accepted socket runs through
//! [`handle_connection`], which loops reading request heads off a per-connection
//! buffer, dispatches to the router (or to async log/admin handlers when the
//! response cannot be produced synchronously), and stops when the client
//! signals close or the per-connection request cap is hit.

use std::time::Duration;

use tokio::io::{AsyncReadExt, BufReader, BufWriter};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;

use crate::web::auth::{classify, AuthOutcome, Role, SsoTransportPolicy};
use crate::web::metrics::write_metrics_response;

use super::router::{dispatch, unauthorized_for};
use super::state::current_options;
use super::wire::{find_double_crlf, write_simple, ParsedRequest, ReadError, Response};

/// Soft cap on requests per keep-alive connection. After this many
/// requests we close so a misbehaving client cannot pin a worker
/// forever; HTTP/1.1 clients that need more will reconnect.
const KEEPALIVE_MAX_REQUESTS: u32 = 1000;

/// Idle timeout between requests on a keep-alive connection. Browsers
/// hold these open for minutes by default; pg_doorman terminates faster
/// because each idle connection still costs an FD and a tokio task.
const KEEPALIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Absolute deadline to finish a request header after the peer has sent
/// at least one byte. This bounds slowloris-style clients that drip bytes
/// without ever completing `\r\n\r\n`.
const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn handle_connection(stream: TcpStream) {
    let peer_addr = stream.peer_addr().ok();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    let mut req_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut handled = 0u32;
    while handled < KEEPALIVE_MAX_REQUESTS {
        // `req_buf` carries over any bytes from the previous read that
        // belonged to the *next* request (clients can pipeline two GETs
        // into one TCP write). `read_request_head` extends it until the
        // header terminator is in view, then we slice off only the
        // first request and keep the tail for the next iteration.
        let head_end = match read_request_head(&mut reader, &mut req_buf).await {
            Ok(0) => return, // peer closed cleanly between requests
            Ok(end) => end,
            Err(ReadError::Io(_)) | Err(ReadError::Idle) => return,
            Err(ReadError::TooLarge) => {
                let _ = write_simple(&mut writer, 431, "Request Header Fields Too Large").await;
                return;
            }
        };
        let started = std::time::Instant::now();
        let head_bytes = &req_buf[..head_end];
        let raw = match std::str::from_utf8(head_bytes) {
            Ok(s) => s,
            Err(_) => {
                let _ = write_simple(&mut writer, 400, "Bad Request").await;
                return;
            }
        };
        let Some(parsed) = ParsedRequest::parse(raw) else {
            let _ = write_simple(&mut writer, 400, "Bad Request").await;
            return;
        };
        let close_after = parsed.connection_close;

        // Pre-compute the access-log fields we need from `parsed` before
        // it goes out of scope.
        let log_method = parsed.method.to_string();
        let log_path = parsed.path.to_string();
        let log_query_present = parsed.query.is_some();
        let opts = current_options();

        let peer_string = crate::web::peer::render_peer(
            peer_addr,
            parsed.x_forwarded_for,
            parsed.forwarded,
            &opts.trusted_proxies,
        );

        // kubernetes liveness/readiness probes - unauthenticated,
        // cheap, no SHOW POOLS walk. Returns 200 immediately on /health
        // (liveness - process is alive and HTTP listener responds) and
        // 200/503 on /ready (readiness - shutdown drain state).
        if parsed.method == "GET" && parsed.path == "/health" {
            let response = Response::ok_text("ok");
            let write_outcome = response.write(&mut writer).await;
            crate::web::access_log::write(
                &log_method,
                &log_path,
                log_query_present,
                write_outcome.status(),
                write_outcome.bytes(),
                started.elapsed().as_millis() as u64,
                &peer_string,
                &AuthOutcome::Anonymous,
            );
            req_buf.drain(..head_end);
            handled += 1;
            if close_after {
                return;
            }
            continue;
        }
        if parsed.method == "GET" && parsed.path == "/ready" {
            // 503 also during STARTUP (READY=false) until pools
            // are loaded AND the main PG listener is accepting. Prevents
            // k8s from routing client traffic to a pod that's still
            // spawning pools.
            let shutting_down =
                crate::app::server::SHUTDOWN_IN_PROGRESS.load(std::sync::atomic::Ordering::Relaxed);
            let ready = crate::app::server::READY.load(std::sync::atomic::Ordering::Acquire);
            let response = if shutting_down {
                Response::service_unavailable("shutting down")
            } else if !ready {
                Response::service_unavailable("starting up")
            } else {
                Response::ok_text("ready")
            };
            let write_outcome = response.write(&mut writer).await;
            crate::web::access_log::write(
                &log_method,
                &log_path,
                log_query_present,
                write_outcome.status(),
                write_outcome.bytes(),
                started.elapsed().as_millis() as u64,
                &peer_string,
                &AuthOutcome::Anonymous,
            );
            req_buf.drain(..head_end);
            handled += 1;
            if close_after {
                return;
            }
            continue;
        }

        // /metrics is always served, regardless of ui_active or auth.
        // It writes its body directly through the gzip-aware response
        // writer, so we don't build a Response struct here.
        if parsed.method == "GET" && parsed.path == "/metrics" {
            let metrics_outcome = write_metrics_response(&mut writer, parsed.accepts_gzip).await;
            crate::web::access_log::write(
                &log_method,
                &log_path,
                log_query_present,
                metrics_outcome.status(),
                metrics_outcome.bytes(),
                started.elapsed().as_millis() as u64,
                &peer_string,
                &AuthOutcome::Anonymous,
            );
            req_buf.drain(..head_end);
            handled += 1;
            if close_after {
                return;
            }
            continue;
        }

        // Decide whether the request reached us over a trusted HTTPS hop
        // before classifying. The transport gate only matters when the
        // operator opted in via `[web].sso_require_https`; when off, the
        // verdict is moot.
        let request_is_secure = crate::web::peer::request_is_secure(
            peer_addr,
            parsed.x_forwarded_proto,
            &opts.trusted_proxies,
        );
        let auth = classify(
            parsed.authorization,
            parsed.cookie,
            extract_query_token(parsed.query),
            &opts.admin_username,
            &opts.admin_password,
            opts.sso.as_deref(),
            SsoTransportPolicy {
                request_is_secure,
                require_https: opts.sso_require_https,
            },
        );

        // /api/logs needs an async handler because it talks to the LogTap
        // consumer task via mpsc + oneshot; the rest of the API stays sync.
        // Pre-screen ui_active and the role here so dispatch() never sees
        // the path on the success branch — on failure we fall through to
        // dispatch() which already returns the right 401/404.
        let response = if opts.ui_active && parsed.method == "GET" && parsed.path == "/api/logs" {
            // /api/logs needs Sso or Admin (personal data); Anonymous
            // and Rejected both yield 401, Sso/Admin proceed.
            if auth.role() < Role::Sso {
                unauthorized_for(&parsed)
            } else {
                let query = crate::web::routes::query::parse_query(parsed.query.unwrap_or(""));
                crate::web::routes::logs::handle_logs(&query).await
            }
        } else if opts.ui_active
            && parsed.method == "POST"
            && parsed.path.starts_with("/api/admin/")
        {
            if !matches!(auth, AuthOutcome::Admin(_)) {
                if matches!(auth, AuthOutcome::Sso(_)) {
                    Response::forbidden("admin role required")
                } else {
                    unauthorized_for(&parsed)
                }
            } else if !crate::web::server::csrf::is_same_origin(
                parsed.origin,
                parsed.referer,
                parsed.host,
                &opts.allowed_admin_origins,
            ) {
                // SSO admin cookies are auto-attached by the browser
                // to cross-origin POSTs. Without a same-origin check an
                // attacker page can fire /api/admin/shutdown while the
                // operator is signed in. Origin/Referer must match Host.
                crate::web::metrics::WEB_ADMIN_CSRF_REJECTED
                    .with_label_values::<&str>(&[])
                    .inc();
                Response::forbidden("cross-origin admin POST rejected")
            } else {
                let raw_admin_path;
                let admin_path = match parsed.query {
                    Some(query) => {
                        raw_admin_path = format!("{}?{}", parsed.path, query);
                        raw_admin_path.as_str()
                    }
                    None => parsed.path,
                };
                crate::web::routes::admin::handle_admin_action(admin_path).await
            }
        } else {
            dispatch(&parsed, &opts, &auth)
        };

        let write_outcome = response.write(&mut writer).await;
        crate::web::access_log::write(
            &log_method,
            &log_path,
            log_query_present,
            write_outcome.status(),
            write_outcome.bytes(),
            started.elapsed().as_millis() as u64,
            &peer_string,
            &auth,
        );

        // Discard the request we just answered; pipelined bytes (a
        // second request that came in the same TCP read) stay at the
        // head of `req_buf` for the next iteration to consume.
        req_buf.drain(..head_end);
        handled += 1;
        if close_after {
            return;
        }
    }
    // Hit the per-connection request cap. Close so the client knows to
    // reconnect rather than queue more behind us.
}

/// Pick `token=<jwt>` out of a raw query string, returning the token
/// substring without URL-decoding. JWTs are base64url so they round-trip
/// through query strings unchanged; if the proxy URL-encoded the token
/// (replacing `+/=`), `SsoRuntime::validate` rejects it and the SPA
/// retries via Bearer header.
fn extract_query_token(query: Option<&str>) -> Option<&str> {
    let q = query?;
    q.split('&').find_map(|pair| pair.strip_prefix("token="))
}

/// Extend `buf` with bytes from the wire until the request-header
/// terminator `\r\n\r\n` is in view. Returns the offset *just past* the
/// terminator (so the caller knows where the headers end and any
/// pipelined body / next request begin), or `Ok(0)` if the peer closed
/// cleanly between requests. Caps the buffer at 32 KiB so a malicious
/// client cannot push us into OOM.
async fn read_request_head(
    reader: &mut BufReader<OwnedReadHalf>,
    buf: &mut Vec<u8>,
) -> Result<usize, ReadError> {
    read_request_head_with_timeouts(reader, buf, KEEPALIVE_IDLE_TIMEOUT, REQUEST_HEADER_TIMEOUT)
        .await
}

async fn read_request_head_with_timeouts(
    reader: &mut BufReader<OwnedReadHalf>,
    buf: &mut Vec<u8>,
    idle_timeout: Duration,
    header_timeout: Duration,
) -> Result<usize, ReadError> {
    const MAX_HEADER_BYTES: usize = 32 * 1024;
    if buf.is_empty() {
        // Wait up to KEEPALIVE_IDLE_TIMEOUT for the first byte; once
        // bytes arrive, header_timeout bounds the rest of the header.
        let mut chunk = [0u8; 1024];
        let read_fut = reader.read(&mut chunk);
        let n = match tokio::time::timeout(idle_timeout, read_fut).await {
            Ok(r) => r?,
            Err(_elapsed) => return Err(ReadError::Idle),
        };
        if n == 0 {
            return Ok(0);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if let Some(end) = find_double_crlf(buf) {
        return Ok(end);
    }

    tokio::time::timeout(
        header_timeout,
        read_request_head_rest(reader, buf, MAX_HEADER_BYTES),
    )
    .await
    .unwrap_or(Err(ReadError::Idle))
}

async fn read_request_head_rest(
    reader: &mut BufReader<OwnedReadHalf>,
    buf: &mut Vec<u8>,
    max_header_bytes: usize,
) -> Result<usize, ReadError> {
    let mut chunk = [0u8; 1024];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            // Peer closed mid-request — treat as malformed.
            return Err(ReadError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF mid request headers",
            )));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_double_crlf(buf) {
            return Ok(end);
        }
        if buf.len() >= max_header_bytes {
            return Err(ReadError::TooLarge);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_query_token_returns_value_when_only_param() {
        assert_eq!(
            extract_query_token(Some("token=abc.def.ghi")),
            Some("abc.def.ghi")
        );
    }

    #[test]
    fn extract_query_token_returns_value_when_among_others() {
        assert_eq!(
            extract_query_token(Some("foo=1&token=jwt&bar=2")),
            Some("jwt")
        );
    }

    #[test]
    fn extract_query_token_handles_trailing_amp() {
        assert_eq!(extract_query_token(Some("token=jwt&")), Some("jwt"));
    }

    #[test]
    fn metrics_access_log_uses_write_outcome() {
        let src = include_str!("http.rs");
        let body = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("http source must contain production body");

        assert!(
            body.contains("let metrics_outcome = write_metrics_response"),
            "/metrics dispatch must keep the direct writer outcome"
        );
        assert!(
            body.contains("metrics_outcome.status()"),
            "/metrics access log must not hard-code status=200 after write failure"
        );
        assert!(
            body.contains("metrics_outcome.bytes()"),
            "/metrics access log must use the writer's emitted body size"
        );
    }

    #[test]
    fn regular_access_log_uses_response_write_outcome() {
        let src = include_str!("http.rs");
        let body = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("http source must contain production body");

        assert!(
            !body.contains("let _ = response.write(&mut writer).await"),
            "regular web/API access logs must not discard Response::write outcome"
        );
        assert!(
            body.contains("write_outcome.status()"),
            "regular web/API access logs must use actual response write status"
        );
        assert!(
            body.contains("write_outcome.bytes()"),
            "regular web/API access logs must use actual response write byte count"
        );
    }

    #[test]
    fn extract_query_token_returns_first_match() {
        // Two `token=` keys would be malformed but the function must be
        // deterministic — the first wins.
        assert_eq!(
            extract_query_token(Some("token=first&token=second")),
            Some("first")
        );
    }

    #[test]
    fn extract_query_token_rejects_keys_with_token_as_substring() {
        // `mytoken=foo` must NOT match — `strip_prefix("token=")` only
        // matches at the start of a pair.
        assert_eq!(extract_query_token(Some("mytoken=foo&other=bar")), None);
    }

    #[test]
    fn extract_query_token_returns_empty_for_token_without_value() {
        assert_eq!(extract_query_token(Some("token=")), Some(""));
    }

    #[test]
    fn extract_query_token_returns_none_for_no_token_key() {
        assert_eq!(extract_query_token(Some("foo=1&bar=2")), None);
    }

    #[test]
    fn extract_query_token_returns_none_for_empty_query() {
        assert_eq!(extract_query_token(Some("")), None);
    }

    #[test]
    fn extract_query_token_returns_none_for_none_input() {
        assert_eq!(extract_query_token(None), None);
    }

    #[tokio::test]
    async fn partial_header_times_out_after_first_byte() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        client.write_all(b"G").await.unwrap();

        let (read_half, _) = server.into_split();
        let mut reader = BufReader::new(read_half);
        let mut buf = Vec::new();

        let result = read_request_head_with_timeouts(
            &mut reader,
            &mut buf,
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .await;

        assert!(matches!(result, Err(ReadError::Idle)));
    }
}
