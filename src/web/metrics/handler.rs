//! Handler that builds the /metrics body and writes it onto a TcpStream.
//! The accept loop and HTTP routing live in `crate::web::server`.

use flate2::write::GzEncoder;
use flate2::Compression;
use log::{error, info, warn};
use once_cell::sync::Lazy;
use prometheus::{Encoder, TextEncoder};
use std::io::Write;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;

use super::metrics::update_metrics;
use super::{REGISTRY, WEB_RESPONSE_WRITE_ERRORS_TOTAL};

/// Above this latency a /metrics response is loud enough that we want to see
/// it in operator logs by default. Below it the timing is still logged, but
/// at INFO so it's part of normal operations rather than an alarm.
const SLOW_RESPONSE_THRESHOLD: Duration = Duration::from_millis(100);

/// Minimum gap between consecutive slow-response WARN lines. Without it a
/// misconfigured scraper (e.g. accidental `scrape_interval: 1s`) under a
/// genuine slow path turns into 1 warn/s, drowning out real signal in the
/// log pipeline. Operators learn nothing from the 2nd, 10th, 100th copy of
/// the same line, so we throttle to one per N seconds.
const SLOW_RESPONSE_LOG_INTERVAL_SECS: i64 = 30;

/// Deadline for each socket write step in the /metrics response. A slow or
/// non-reading client should not pin a web task and FD indefinitely after the
/// expensive scrape body has already been produced.
const METRICS_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const METRICS_WRITE_FAILURE_STATUS: u16 = 499;
const METRICS_INTERNAL_ERROR_STATUS: u16 = 500;

/// Unix-epoch second of the last slow-response WARN emitted from this path.
/// Shared across all in-flight `/metrics` requests so the gate is global.
static SLOW_RESPONSE_LAST_WARN: AtomicI64 = AtomicI64::new(0);

/// Serializes the scrape snapshot section. Several metric families are rebuilt
/// from mutable gauges or process counters immediately before `REGISTRY.gather()`;
/// concurrent scrapes can otherwise interleave reset/update/gather phases and
/// emit duplicate deltas or partial label cleanup.
static METRICS_SCRAPE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MetricsResponseOutcome {
    status: u16,
    bytes: usize,
}

impl MetricsResponseOutcome {
    fn success(bytes: usize) -> Self {
        Self { status: 200, bytes }
    }

    fn write_failure() -> Self {
        Self {
            status: METRICS_WRITE_FAILURE_STATUS,
            bytes: 0,
        }
    }

    fn internal_error() -> Self {
        Self {
            status: METRICS_INTERNAL_ERROR_STATUS,
            bytes: 0,
        }
    }

    pub(crate) fn status(self) -> u16 {
        self.status
    }

    pub(crate) fn bytes(self) -> usize {
        self.bytes
    }
}

async fn write_all_with_timeout(
    writer: &mut BufWriter<OwnedWriteHalf>,
    bytes: &[u8],
    segment: &'static str,
) -> bool {
    match tokio::time::timeout(METRICS_WRITE_TIMEOUT, writer.write_all(bytes)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            record_metrics_response_write_error(segment, "io");
            error!("Failed to write metrics {segment}: {e}");
            false
        }
        Err(_) => {
            record_metrics_response_write_error(segment, "timeout");
            warn!("Timed out writing metrics {segment} after {METRICS_WRITE_TIMEOUT:?}");
            false
        }
    }
}

async fn flush_with_timeout(writer: &mut BufWriter<OwnedWriteHalf>) -> bool {
    match tokio::time::timeout(METRICS_WRITE_TIMEOUT, writer.flush()).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            record_metrics_response_write_error("flush", "io");
            error!("Failed to flush metrics connection: {e}");
            false
        }
        Err(_) => {
            record_metrics_response_write_error("flush", "timeout");
            warn!("Timed out flushing metrics connection after {METRICS_WRITE_TIMEOUT:?}");
            false
        }
    }
}

fn record_metrics_response_write_error(stage: &'static str, reason: &'static str) {
    WEB_RESPONSE_WRITE_ERRORS_TOTAL
        .with_label_values(&[stage, reason])
        .inc();
}

/// Builds the Prometheus metrics body and writes a complete HTTP/1.1 response
/// onto the supplied writer. The mux must have already parsed the request
/// (this function performs no reads on the socket).
pub(crate) async fn write_metrics_response(
    writer: &mut BufWriter<OwnedWriteHalf>,
    accepts_gzip: bool,
) -> MetricsResponseOutcome {
    let started = Instant::now();

    let encoder = TextEncoder::new();
    let metric_families = {
        let _scrape_guard = METRICS_SCRAPE_LOCK.lock().await;
        update_metrics();
        REGISTRY.gather()
    };
    let mut buffer = Vec::new();

    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        error!("Failed to encode metrics: {e}");
        return MetricsResponseOutcome::internal_error();
    }

    let content_type = encoder.format_type();

    let (response_body, content_encoding) = if accepts_gzip {
        let mut compressed = Vec::new();
        {
            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            if let Err(e) = gz.write_all(&buffer) {
                error!("Failed to compress metrics data: {e}");
                return MetricsResponseOutcome::internal_error();
            }
            if let Err(e) = gz.finish() {
                error!("Failed to finish gzip compression: {e}");
                return MetricsResponseOutcome::internal_error();
            }
        }
        (compressed, "Content-Encoding: gzip\r\n")
    } else {
        (buffer, "")
    };

    let body_len = response_body.len();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{content_encoding}Content-Length: {body_len}\r\n\r\n"
    );

    if !write_all_with_timeout(writer, response.as_bytes(), "header").await {
        return MetricsResponseOutcome::write_failure();
    }
    if !write_all_with_timeout(writer, &response_body, "body").await {
        return MetricsResponseOutcome::write_failure();
    }
    if !flush_with_timeout(writer).await {
        return MetricsResponseOutcome::write_failure();
    }

    // Log every /metrics request at INFO so operators see in the normal log
    // stream how often Prometheus is scraping and how long each call takes
    // (the typical question after a p99 regression is "did scrape get
    // slower"). Above SLOW_RESPONSE_THRESHOLD the same event is also raised
    // to WARN, rate-limited to one warn per
    // `SLOW_RESPONSE_LOG_INTERVAL_SECS` so a misbehaving scraper does not
    // turn it into a per-request flood.
    let elapsed = started.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    info!("/metrics request handled in {elapsed_ms:.1} ms (bytes={body_len}, gzip={accepts_gzip})");
    if elapsed >= SLOW_RESPONSE_THRESHOLD {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let last = SLOW_RESPONSE_LAST_WARN.load(Ordering::Relaxed);
        if now_secs - last >= SLOW_RESPONSE_LOG_INTERVAL_SECS
            && SLOW_RESPONSE_LAST_WARN
                .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            warn!(
                "/metrics request slow: {elapsed_ms:.1} ms \
                 (bytes={body_len}, gzip={accepts_gzip}, rate-limited 1/{SLOW_RESPONSE_LOG_INTERVAL_SECS}s)"
            );
        }
    }
    MetricsResponseOutcome::success(body_len)
}

#[cfg(test)]
mod tests {
    #[test]
    fn update_and_gather_stay_in_one_serialized_scrape_section() {
        let src = include_str!("handler.rs");
        let start = src
            .find("pub(crate) async fn write_metrics_response")
            .expect("write_metrics_response should exist");
        let body = &src[start..];
        let end = body
            .find("\n#[cfg(test)]")
            .expect("test module should follow write_metrics_response");
        let body = &body[..end];
        let lock = body
            .find("METRICS_SCRAPE_LOCK.lock")
            .expect("metrics scrape path must acquire the scrape lock");
        let update = body
            .find("update_metrics();")
            .expect("metrics scrape path should update metrics before gather");
        let gather = body
            .find("REGISTRY.gather()")
            .expect("metrics scrape path should gather the registry");

        assert!(
            lock < update && update < gather,
            "update_metrics() and REGISTRY.gather() must run under one scrape lock"
        );
    }

    #[test]
    fn metrics_socket_writes_are_deadline_bound() {
        let src = include_str!("handler.rs");
        let start = src
            .find("pub(crate) async fn write_metrics_response")
            .expect("write_metrics_response should exist");
        let body = &src[start..];
        let end = body
            .find("\n#[cfg(test)]")
            .expect("test module should follow write_metrics_response");
        let body = &body[..end];

        assert!(
            body.contains("write_all_with_timeout(writer, response.as_bytes(), \"header\")"),
            "metrics header write must use a bounded write helper"
        );
        assert!(
            body.contains("write_all_with_timeout(writer, &response_body, \"body\")"),
            "metrics body write must use a bounded write helper"
        );
        assert!(
            body.contains("flush_with_timeout(writer)"),
            "metrics flush must use a bounded flush helper"
        );
    }

    #[test]
    fn metrics_response_write_errors_are_counted() {
        let src = include_str!("handler.rs");
        let body = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("handler source must contain production body");

        assert!(
            body.contains("record_metrics_response_write_error(segment, \"io\")"),
            "metrics response write I/O errors must increment a bounded counter"
        );
        assert!(
            body.contains("record_metrics_response_write_error(segment, \"timeout\")"),
            "metrics response write timeouts must increment a bounded counter"
        );
        assert!(
            body.contains("record_metrics_response_write_error(\"flush\", \"io\")"),
            "metrics response flush I/O errors must increment a bounded counter"
        );
        assert!(
            body.contains("record_metrics_response_write_error(\"flush\", \"timeout\")"),
            "metrics response flush timeouts must increment a bounded counter"
        );
    }

    #[test]
    fn metrics_response_reports_access_log_outcome() {
        let src = include_str!("handler.rs");
        let body = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("handler source must contain production body");

        assert!(
            body.contains("pub(crate) struct MetricsResponseOutcome"),
            "metrics writer must expose response status/bytes for access logging"
        );
        assert!(
            body.contains("-> MetricsResponseOutcome"),
            "metrics writer must return response outcome instead of ()"
        );
    }
}
