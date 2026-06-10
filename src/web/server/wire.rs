//! Request parsing and response serialization. The wire layer has no
//! knowledge of routing, auth, or static-asset semantics beyond cache
//! headers — it just turns bytes into [`ParsedRequest`] and a
//! [`Response`] back into bytes.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedWriteHalf;

use crate::web::metrics::WEB_RESPONSE_WRITE_ERRORS_TOTAL;

/// Deadline for each socket write step in regular web/admin responses.
const WEB_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const WEB_RESPONSE_WRITE_FAILURE_STATUS: u16 = 499;

#[derive(Debug)]
pub(super) enum ReadError {
    /// Underlying socket error. We do not track the inner error because
    /// the only action on Io is "close the connection" — same as Idle.
    #[allow(dead_code)]
    Io(std::io::Error),
    Idle,
    TooLarge,
}

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        ReadError::Io(e)
    }
}

/// Index of the byte immediately after the first `\r\n\r\n` sequence,
/// or `None` if the buffer does not yet contain the terminator.
pub(super) fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

#[derive(Debug)]
pub(super) struct ParsedRequest<'a> {
    pub(super) method: &'a str,
    /// Path with the query string stripped off. `path` for
    /// `GET /api/logs?since=10` is `/api/logs`; the `since=10` part lives
    /// in [`ParsedRequest::query`].
    pub(super) path: &'a str,
    /// Substring of the original path after `?`, if any. The mux scans
    /// this for `?token=<jwt>` (SSO query source).
    pub(super) query: Option<&'a str>,
    pub(super) authorization: Option<&'a str>,
    /// Raw value of the `Cookie:` header, if present. The mux walks this
    /// for `sso_access_token=...` (SSO cookie source).
    pub(super) cookie: Option<&'a str>,
    /// Raw value of the `X-Forwarded-For:` header, if present. Used by
    /// the access-log resolver when the listener sits behind a trusted
    /// reverse proxy.
    pub(super) x_forwarded_for: Option<&'a str>,
    /// Raw value of the `Forwarded:` header (RFC 7239). Same role as
    /// `x_forwarded_for`; both are walked.
    pub(super) forwarded: Option<&'a str>,
    /// Raw value of the `X-Forwarded-Proto:` header, if present. Only
    /// trusted when the TCP peer is in `[web].trusted_proxies`; used to
    /// gate SSO credentials behind HTTPS when
    /// `[web].sso_require_https = true`.
    pub(super) x_forwarded_proto: Option<&'a str>,
    pub(super) accepts_gzip: bool,
    /// True when the request advertises `Accept: application/json`. The SPA
    /// `fetch()` wrapper sets this on every call; a browser hitting the URL
    /// directly would not. The mux uses it to skip the `WWW-Authenticate`
    /// header on 401 — otherwise the browser caches whatever the user typed
    /// in its native basic-auth dialog and replays it forever, hiding our
    /// React sign-in modal.
    pub(super) accepts_json: bool,
    /// True when the request explicitly opts out of HTTP/1.1 keep-alive
    /// (`Connection: close`) or speaks an older HTTP version. The mux
    /// uses it to decide whether to drop the connection after the
    /// response or wait for another request on the same socket.
    pub(super) connection_close: bool,
    /// Raw value of the `Origin:` header, if present. Used by the CSRF
    /// gate on POST /api/admin/* - the mutation is rejected when an
    /// authenticated request originates from a different origin than the
    /// configured listener Host.
    pub(super) origin: Option<&'a str>,
    /// Raw value of the `Referer:` header, if present. Fallback when the
    /// browser stripped the Origin header (legacy navigation flows).
    pub(super) referer: Option<&'a str>,
    /// Raw value of the `Host:` header, if present. Compared against
    /// Origin/Referer on POST /api/admin/* to enforce same-origin.
    pub(super) host: Option<&'a str>,
}

impl<'a> ParsedRequest<'a> {
    pub(super) fn parse(raw: &'a str) -> Option<Self> {
        let mut lines = raw.split("\r\n");
        let request_line = lines.next()?;
        let mut parts = request_line.splitn(3, ' ');
        let method = parts.next()?;
        let raw_path = parts.next()?;
        let http_version = parts.next()?;
        let (path, query) = match raw_path.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (raw_path, None),
        };

        let mut authorization = None;
        let mut cookie = None;
        let mut x_forwarded_for = None;
        let mut x_forwarded_proto = None;
        let mut forwarded = None;
        let mut accepts_gzip = false;
        let mut accepts_json = false;
        let mut connection_close = !http_version.eq_ignore_ascii_case("HTTP/1.1");
        let mut origin = None;
        let mut referer = None;
        let mut host = None;
        for line in lines {
            if line.is_empty() {
                break;
            }
            // Headers are case-insensitive per RFC 7230. Match by case-
            // insensitive prefix without allocating a lowercase copy of
            // the header value.
            if let Some(value) = strip_header_prefix(line, "Authorization") {
                authorization = Some(value);
            } else if let Some(value) = strip_header_prefix(line, "Cookie") {
                cookie = Some(value);
            } else if let Some(value) = strip_header_prefix(line, "X-Forwarded-For") {
                x_forwarded_for = Some(value);
            } else if let Some(value) = strip_header_prefix(line, "X-Forwarded-Proto") {
                x_forwarded_proto = Some(value);
            } else if let Some(value) = strip_header_prefix(line, "Forwarded") {
                forwarded = Some(value);
            } else if let Some(value) = strip_header_prefix(line, "Accept-Encoding") {
                if contains_ascii_ci(value, "gzip") {
                    accepts_gzip = true;
                }
            } else if let Some(value) = strip_header_prefix(line, "Accept") {
                if contains_ascii_ci(value, "application/json") {
                    accepts_json = true;
                }
            } else if let Some(value) = strip_header_prefix(line, "Connection") {
                if contains_ascii_ci(value, "close") {
                    connection_close = true;
                }
            } else if let Some(value) = strip_header_prefix(line, "Origin") {
                origin = Some(value);
            } else if let Some(value) = strip_header_prefix(line, "Referer") {
                referer = Some(value);
            } else if let Some(value) = strip_header_prefix(line, "Host") {
                host = Some(value);
            }
        }
        Some(ParsedRequest {
            method,
            path,
            query,
            authorization,
            cookie,
            x_forwarded_for,
            x_forwarded_proto,
            forwarded,
            accepts_gzip,
            accepts_json,
            connection_close,
            origin,
            referer,
            host,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) reason: &'static str,
    pub(crate) extra_headers: Vec<(&'static str, String)>,
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResponseWriteOutcome {
    status: u16,
    bytes: usize,
}

impl ResponseWriteOutcome {
    fn success(status: u16, bytes: usize) -> Self {
        Self { status, bytes }
    }

    fn write_failure() -> Self {
        Self {
            status: WEB_RESPONSE_WRITE_FAILURE_STATUS,
            bytes: 0,
        }
    }

    pub(super) fn status(self) -> u16 {
        self.status
    }

    pub(super) fn bytes(self) -> usize {
        self.bytes
    }
}

impl Response {
    pub(crate) fn status(status: u16, reason: &'static str) -> Self {
        Response {
            status,
            reason,
            extra_headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub(crate) fn json(status: u16, reason: &'static str, body: &str) -> Self {
        Response {
            status,
            reason,
            extra_headers: vec![("Content-Type", "application/json".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    /// 401 with `WWW-Authenticate`. Use only for non-JSON callers (curl,
    /// direct browser navigation) — the SPA path uses `unauthorized_silent`
    /// to keep the browser from caching credentials we did not solicit.
    pub(crate) fn unauthorized() -> Self {
        let mut r = Response::status(401, "Unauthorized");
        r.extra_headers.push((
            "WWW-Authenticate",
            "Basic realm=\"pg_doorman admin\"".into(),
        ));
        r
    }

    /// 401 without `WWW-Authenticate`. Use for SPA / JSON callers so the
    /// browser does not cache rejected credentials and replay them under
    /// our React modal.
    pub(crate) fn unauthorized_silent() -> Self {
        Response::status(401, "Unauthorized")
    }

    /// minimal 200 OK with a plaintext body. Used for k8s
    /// liveness/readiness probes - no JSON envelope so probe failures
    /// remain visible in `kubectl describe pod` without parsing.
    pub(crate) fn ok_text(body: &'static str) -> Self {
        Response {
            status: 200,
            reason: "OK",
            extra_headers: vec![("Content-Type", "text/plain; charset=utf-8".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    /// 503 for k8s readiness during shutdown drain. k8s removes
    /// the pod from the Service endpoint list on 503 even though the
    /// pod itself stays alive long enough to drain in-flight clients.
    pub(crate) fn service_unavailable(body: &'static str) -> Self {
        Response {
            status: 503,
            reason: "Service Unavailable",
            extra_headers: vec![("Content-Type", "text/plain; charset=utf-8".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    /// 403 with a small JSON envelope the SPA can render as a "needs
    /// admin role" toast. No `WWW-Authenticate` — the credentials are
    /// valid, just insufficient for this path, so the auth modal must
    /// stay closed.
    pub(crate) fn forbidden(reason: &'static str) -> Self {
        let body = format!(r#"{{"error":"forbidden","message":"{reason}"}}"#);
        Response {
            status: 403,
            reason: "Forbidden",
            extra_headers: vec![("Content-Type", "application/json".into())],
            body: body.into_bytes(),
        }
    }

    /// Serves a static asset (SPA bundle file). Hashed assets get a long
    /// immutable cache; the SPA shell (`index.html`) is no-cache so a redeploy
    /// reaches operators on their next reload. When the caller advertises
    /// `Accept-Encoding: gzip` and the asset compresses worthwhile (text-like
    /// MIME, > 256 bytes), the body is gzipped on the fly — that turns the
    /// ~280 KB JS bundle into ~95 KB on the wire.
    pub(crate) fn static_asset(
        asset: &crate::web::static_assets::Asset,
        accepts_gzip: bool,
    ) -> Self {
        let cache = if asset.immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        let mut headers = vec![
            ("Content-Type", asset.mime.into()),
            ("Cache-Control", cache.into()),
        ];
        // The bundle stores compressible assets pre-gzipped (post-build
        // step) — that keeps the binary ~270 kB smaller than embedding raw
        // text and lets the browser get the bytes verbatim. Clients that
        // don't advertise gzip (rare: curl without `--compressed`, headless
        // probes) get an on-the-fly flate2 decode.
        let body = if asset.pre_gzipped {
            if accepts_gzip {
                headers.push(("Content-Encoding", "gzip".into()));
                asset.bytes.to_vec()
            } else {
                decompress_gzip(asset.bytes).unwrap_or_else(|_| asset.bytes.to_vec())
            }
        } else {
            asset.bytes.to_vec()
        };
        Response {
            status: 200,
            reason: "OK",
            extra_headers: headers,
            body,
        }
    }

    /// Override the status line on a Response built via [`Response::ok_json`].
    /// Useful when the body shape is the same JSON envelope but the
    /// outcome should travel back as 4xx/5xx.
    pub(crate) fn with_status(mut self, status: u16, reason: &'static str) -> Self {
        self.status = status;
        self.reason = reason;
        self
    }

    /// Append (or overwrite) an HTTP header on a Response in flight.
    /// Used by live-data endpoints (`/api/overview`, `/api/events`) to
    /// pin `Cache-Control: no-store` so intermediate proxies and the
    /// browser never collapse two consecutive polls into one stale
    /// response.
    pub(crate) fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.extra_headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        self.extra_headers.push((name, value.into()));
        self
    }

    pub(crate) fn ok_json<T: serde::Serialize>(value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Response {
                status: 200,
                reason: "OK",
                extra_headers: vec![("Content-Type", "application/json".into())],
                body,
            },
            Err(e) => {
                log::error!("Failed to serialize JSON response: {e}");
                Response::status(500, "Internal Server Error")
            }
        }
    }

    pub(super) async fn write(
        self,
        writer: &mut BufWriter<OwnedWriteHalf>,
    ) -> ResponseWriteOutcome {
        let status = self.status;
        let bytes = self.body.len();
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
            self.status,
            self.reason,
            self.body.len()
        );
        // defence-in-depth HTTP security headers on every
        // admin / metrics / SPA response. Without them an XSS in the
        // SPA render path (logs page, /api/top/prepared statement
        // names, etc.) becomes a full admin-account takeover via the
        // same-origin /api/admin/* mutations (no CSP). Frame-jacking
        // an authenticated tab works against the admin POST endpoints
        // because nothing forbids being framed. Minimal set:
        //   X-Frame-Options: DENY        - kill clickjacking
        //   X-Content-Type-Options: nosniff - kill /api/logs sniffing
        //   Referrer-Policy: no-referrer   - kill SSO token URL leakage
        //   Content-Security-Policy: strict default-src - kill XSS pivots
        head.push_str("X-Frame-Options: DENY\r\n");
        head.push_str("X-Content-Type-Options: nosniff\r\n");
        head.push_str("Referrer-Policy: no-referrer\r\n");
        head.push_str(
            "Content-Security-Policy: default-src 'self'; \
             script-src 'self' 'unsafe-inline'; \
             style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; \
             object-src 'none'; \
             frame-ancestors 'none'\r\n",
        );
        for (k, v) in &self.extra_headers {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        if write_all_with_timeout(writer, head.as_bytes(), "header")
            .await
            .is_err()
        {
            return ResponseWriteOutcome::write_failure();
        }
        if !self.body.is_empty()
            && write_all_with_timeout(writer, &self.body, "body")
                .await
                .is_err()
        {
            return ResponseWriteOutcome::write_failure();
        }
        if flush_with_timeout(writer).await.is_err() {
            return ResponseWriteOutcome::write_failure();
        }
        ResponseWriteOutcome::success(status, bytes)
    }
}

async fn write_all_with_timeout(
    writer: &mut BufWriter<OwnedWriteHalf>,
    bytes: &[u8],
    segment: &'static str,
) -> io::Result<()> {
    match tokio::time::timeout(WEB_RESPONSE_WRITE_TIMEOUT, writer.write_all(bytes)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            record_web_response_write_error(segment, "io");
            log::error!("Failed to write web response {segment}: {e}");
            Err(e)
        }
        Err(_) => {
            record_web_response_write_error(segment, "timeout");
            log::warn!(
                "Timed out writing web response {segment} after {WEB_RESPONSE_WRITE_TIMEOUT:?}"
            );
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "web response write timed out",
            ))
        }
    }
}

async fn flush_with_timeout(writer: &mut BufWriter<OwnedWriteHalf>) -> io::Result<()> {
    match tokio::time::timeout(WEB_RESPONSE_WRITE_TIMEOUT, writer.flush()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            record_web_response_write_error("flush", "io");
            log::error!("Failed to flush web response: {e}");
            Err(e)
        }
        Err(_) => {
            record_web_response_write_error("flush", "timeout");
            log::warn!("Timed out flushing web response after {WEB_RESPONSE_WRITE_TIMEOUT:?}");
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "web response flush timed out",
            ))
        }
    }
}

fn record_web_response_write_error(stage: &'static str, reason: &'static str) {
    WEB_RESPONSE_WRITE_ERRORS_TOTAL
        .with_label_values(&[stage, reason])
        .inc();
}

pub(super) async fn write_simple(
    writer: &mut BufWriter<OwnedWriteHalf>,
    status: u16,
    reason: &'static str,
) -> ResponseWriteOutcome {
    Response::status(status, reason).write(writer).await
}

/// Strip a case-insensitive `Header: ` prefix (header name + `: `)
/// without allocating. Returns the header value when the prefix matches,
/// `None` otherwise. ASCII-only by design — HTTP header names are
/// strictly ASCII per RFC 7230.
fn strip_header_prefix<'a>(line: &'a str, header: &str) -> Option<&'a str> {
    let need = header.len() + 2; // ": "
    let bytes = line.as_bytes();
    if bytes.len() < need {
        return None;
    }
    if !line.as_bytes()[..header.len()].eq_ignore_ascii_case(header.as_bytes()) {
        return None;
    }
    if &bytes[header.len()..need] != b": " {
        return None;
    }
    Some(&line[need..])
}

/// Case-insensitive `contains` over ASCII bytes without allocation.
fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Decompress a pre-gzipped asset for the rare client that does not
/// advertise gzip. Compressible assets are pre-gzipped at build time so
/// the binary ships only the compressed form; clients that omit
/// `Accept-Encoding: gzip` (curl without `--compressed`, plain probes)
/// pay this decode once per request, which is acceptable because the
/// console is a low-traffic operator surface.
fn decompress_gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::with_capacity(bytes.len() * 4);
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn web_response_socket_writes_are_deadline_bound() {
        let src = include_str!("wire.rs");
        let start = src
            .find("    pub(super) async fn write(\n        self,\n        writer:")
            .expect("Response::write should exist");
        let body = &src[start..];
        let end = body
            .find("\n}\n\nasync fn write_all_with_timeout")
            .expect("bounded write helper should follow Response::write");
        let body = &body[..end];

        assert!(
            body.contains("write_all_with_timeout(writer, head.as_bytes(), \"header\")"),
            "web response header write must use a bounded write helper"
        );
        assert!(
            body.contains("write_all_with_timeout(writer, &self.body, \"body\")"),
            "web response body write must use a bounded write helper"
        );
        assert!(
            body.contains("flush_with_timeout(writer)"),
            "web response flush must use a bounded flush helper"
        );
    }

    #[test]
    fn web_response_write_errors_are_logged_and_counted() {
        let src = include_str!("wire.rs");

        assert!(
            src.contains("record_web_response_write_error(segment, \"io\")"),
            "web response write I/O errors must increment a bounded counter"
        );
        assert!(
            src.contains("record_web_response_write_error(segment, \"timeout\")"),
            "web response write timeouts must increment a bounded counter"
        );
        assert!(
            src.contains("record_web_response_write_error(\"flush\", \"io\")"),
            "web response flush I/O errors must increment a bounded counter"
        );
        assert!(
            src.contains("record_web_response_write_error(\"flush\", \"timeout\")"),
            "web response flush timeouts must increment a bounded counter"
        );
        assert!(
            src.contains("Failed to write web response {segment}: {e}"),
            "web response write I/O errors must be logged"
        );
        assert!(
            src.contains("Failed to flush web response: {e}"),
            "web response flush I/O errors must be logged"
        );
    }
}
