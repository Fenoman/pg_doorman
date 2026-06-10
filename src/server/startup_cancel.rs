use std::time::Duration;

use bytes::{BufMut, BytesMut};
use log::warn;

use crate::config::get_config;
use crate::config::tls::{ServerTlsConfig, ServerTlsMode};
use crate::errors::Error;
use crate::messages::constants::CANCEL_REQUEST_CODE;
use crate::messages::write_all_flush;

use super::stream::{create_tcp_stream_inner, create_unix_stream_inner};

/// cap the cancel pipeline (TCP connect + optional
/// TLS handshake + 16-byte write_all_flush) at `general.connect_timeout`
/// with a floor of 5s. The legacy shape awaited each stage unbounded;
/// a hung backend host could pin every cancel-handler task indefinitely
/// and arbitrarily widen the cancel-routing TOCTOU window (`Server::Drop`
/// can clear `CANCELED_PIDS` while a stale cancel is still in flight,
/// allowing the cancel to land on whoever owns the recycled pid next).
/// 5s is a generous upper bound - a healthy PG host completes the
/// cancel handshake in single-digit ms; >5s implies the host is gone
/// or the network is partitioned, and the cancel was going to miss
/// anyway.
const CANCEL_PIPELINE_FLOOR: Duration = Duration::from_secs(5);

/// Issue a query cancellation request to the server.
/// Uses a separate connection that's not part of the connection pool.
/// When the original connection used TLS, the cancel connection also uses TLS.
pub(crate) async fn cancel(
    host: &str,
    port: u16,
    process_id: i32,
    secret_key: i32,
    server_tls: &ServerTlsConfig,
    connected_with_tls: bool,
    pool_name: &str,
) -> Result<(), Error> {
    let disable_config = ServerTlsConfig {
        mode: ServerTlsMode::Disable,
        connector: None,
        cert_hash: None,
    };
    let cancel_tls = if connected_with_tls {
        server_tls
    } else {
        &disable_config
    };

    // Read once - `get_config()` snapshots an `Arc<Config>`, cheap.
    // Falls back to the floor if the operator configured a smaller
    // value, so the cancel pipeline always has at least 5s to complete
    // (DNS, slow TLS handshake on a healthy host).
    let configured = get_config().general.connect_timeout.as_std();
    let deadline = std::cmp::max(configured, CANCEL_PIPELINE_FLOOR);

    let result: Result<Result<(), Error>, tokio::time::error::Elapsed> =
        tokio::time::timeout(deadline, async {
            let mut stream = if host.starts_with('/') {
                create_unix_stream_inner(host, port).await?
            } else {
                create_tcp_stream_inner(host, port, cancel_tls, pool_name).await?
            };

            warn!("cancel request forwarded to {host}:{port} pid={process_id}");

            let mut bytes = BytesMut::with_capacity(16);
            bytes.put_i32(16);
            bytes.put_i32(CANCEL_REQUEST_CODE);
            bytes.put_i32(process_id);
            bytes.put_i32(secret_key);

            write_all_flush(&mut stream, &bytes).await
        })
        .await;

    match result {
        Ok(inner) => inner,
        Err(_) => {
            warn!(
                "cancel pipeline to {host}:{port} pid={process_id} timed out after {}s - abandoning",
                deadline.as_secs()
            );
            Err(Error::SocketError(format!(
                "cancel timeout to {host}:{port} pid={process_id}"
            )))
        }
    }
}
