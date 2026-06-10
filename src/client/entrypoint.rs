use log::{error, info, warn};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::sync::atomic::Ordering;
use tokio::io::{split, AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};

use crate::config::config_arc;
use crate::errors::Error;
use crate::messages::config_socket::configure_tcp_socket_for_cancel;
use crate::messages::{error_response_terminal, write_all_flush};
use crate::pool::ClientServerMap;
use crate::stats::{CANCEL_CONNECTION_COUNTER, PLAIN_CONNECTION_COUNTER, TLS_CONNECTION_COUNTER};
use crate::utils::rate_limit::RateLimiter;

use crate::transport::ClientTransport;

use super::core::Client;
use super::startup::{
    get_startup, startup_tls, startup_with_auth_timeout, ClientConnectionType,
    PRE_AUTH_CLIENT_TIMEOUT,
};

#[cfg(not(test))]
const PRE_AUTH_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[cfg(test)]
const PRE_AUTH_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

async fn get_startup_with_pre_auth_timeout<S>(
    stream: &mut S,
) -> Result<(ClientConnectionType, bytes::BytesMut), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout(PRE_AUTH_STARTUP_TIMEOUT, get_startup::<S>(stream)).await {
        Ok(result) => result,
        Err(_) => {
            crate::web::metrics::record_listener_rejection("startup_read_timeout");
            Err(Error::ClientBadStartup)
        }
    }
}

/// Identity info returned from client_entrypoint for disconnect logging.
pub struct ClientSessionInfo {
    pub username: String,
    pub pool_name: String,
    pub connection_id: u64,
}

/// Drive the authenticated-client lifecycle for any transport.
///
/// Three places (plain TCP startup, TCP plain-continue after rejected TLS,
/// and Unix socket startup) used to inline the same sequence: call
/// `Client::startup`, log "client connected", run `client.handle()`, and
/// flush `disconnect_stats` on a late error. Centralising it here keeps
/// the three call sites down to a single generic hop and removes ~90
/// lines of copy-paste.
#[allow(clippy::too_many_arguments)]
async fn drive_authenticated_client<S, T>(
    read: S,
    write: T,
    transport: ClientTransport,
    bytes: bytes::BytesMut,
    client_server_map: ClientServerMap,
    admin_only: bool,
    connection_id: u64,
    #[cfg(unix)] raw_fd: Option<std::os::unix::io::RawFd>,
    #[cfg(all(unix, feature = "tls-migration"))] ssl_ptr: Option<crate::client::core::SslRawPtr>,
    log_client_connections: bool,
    log_label: &'static str,
) -> Result<Option<ClientSessionInfo>, Error>
where
    S: tokio::io::AsyncRead + Unpin + Send + 'static,
    T: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let peer = transport.peer_display();
    match startup_with_auth_timeout(
        read,
        write,
        transport,
        bytes,
        client_server_map,
        admin_only,
        connection_id,
        #[cfg(unix)]
        raw_fd,
        #[cfg(all(unix, feature = "tls-migration"))]
        ssl_ptr,
        PRE_AUTH_CLIENT_TIMEOUT,
    )
    .await
    {
        Ok(mut client) => {
            if log_client_connections {
                info!(
                    "[{}@{} #c{}] client connected from {} ({})",
                    client.username, client.pool_name, client.connection_id, peer, log_label,
                );
            }
            let session_info = ClientSessionInfo {
                username: client.username.clone(),
                pool_name: client.pool_name.clone(),
                connection_id: client.connection_id,
            };
            let result = client.handle().await;
            if !client.is_admin() && result.is_err() {
                client.disconnect_stats();
            }
            result.map(|_| Some(session_info))
        }
        Err(err) => Err(err),
    }
}

pub async fn client_entrypoint_too_many_clients_already(
    mut stream: TcpStream,
    client_server_map: ClientServerMap,
) -> Result<(), Error> {
    crate::web::metrics::record_listener_rejection("too_many_clients");
    let addr = match stream.peer_addr() {
        Ok(addr) => addr,
        Err(err) => {
            return Err(Error::SocketError(format!(
                "Failed to get peer address: {err:?}"
            )));
        }
    };

    match get_startup_with_pre_auth_timeout::<TcpStream>(&mut stream).await {
        Ok((ClientConnectionType::Tls, _)) => {
            write_all_flush(&mut stream, b"N").await?;
            // здесь может быть ошибка SSL is not enabled on the server,
            // вместо too many client, но это сделано намерянно, потому что мы
            // не сможем обработать столько клиентов еще и через SSL.
            //
            // a libpq cancel socket opened with the default
            // `sslmode=prefer` first sends `SSLRequest`, reads our `'N'`,
            // then sends `CancelRequest` over the same plain socket - even
            // when we are at `max_client_connections`. A CancelRequest must
            // be honored regardless of capacity: a cancel connection is
            // ephemeral (it forwards the cancel on a separate short-lived
            // socket and closes) and occupies no pool/client slot. Mirror
            // the normal post-SSL-reject routing beeaea7 added in
            // `client_entrypoint` instead of dropping the cancel by falling
            // straight through to the 53300 rejection.
            match get_startup_with_pre_auth_timeout::<TcpStream>(&mut stream).await {
                Ok((ClientConnectionType::CancelQuery, bytes)) => {
                    CANCEL_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
                    configure_tcp_socket_for_cancel(&stream);
                    let (read, write) = split(stream);
                    return match Client::cancel(read, write, addr, bytes, client_server_map).await {
                        Ok(mut client) => {
                            info!("Cancel request from {addr} (post-SSL-reject, over-limit)");
                            let result = client.handle().await;
                            if !client.is_admin() && result.is_err() {
                                client.disconnect_stats();
                            }
                            result
                        }
                        Err(err) => Err(err),
                    };
                }
                // Client accepted the unencrypted offer and sent a real
                // StartupMessage, or repeated the SSLRequest: in both cases
                // we are still over the limit, so report the 53300 below.
                Ok(_) => (),
                Err(err) => return Err(err),
            }
        }
        Ok((ClientConnectionType::Startup, _)) => (
            // pass
        ),
        Ok((ClientConnectionType::CancelQuery, bytes)) => {
            // Important: without configuring the TCP socket for cancel requests,
            // libpq-based clients (e.g., psycopg2) may emit a noisy stderr warning on cancellation
            // such as:
            // "query cancellation failed: cancellation failed: connection to server ..."
            // We set the appropriate socket options to avoid this spurious message.
            configure_tcp_socket_for_cancel(&stream);
            let (read, write) = split(stream);
            // Continue with cancel query request.
            return match Client::cancel(read, write, addr, bytes, client_server_map).await {
                Ok(mut client) => {
                    info!("Cancel request from {addr}");
                    let result = client.handle().await;
                    if !client.is_admin() && result.is_err() {
                        client.disconnect_stats();
                    }
                    result
                }
                Err(err) => Err(err),
            };
        }
        Err(err) => return Err(err),
    }
    error_response_terminal(&mut stream, "sorry, too many clients already", "53300").await?;
    Ok(())
}

/// Reject an inbound Unix socket client with a proper PostgreSQL ErrorResponse.
///
/// The TCP rejection path above also handles TLS and cancel-request startups,
/// neither of which applies here: Unix sockets do not negotiate TLS, and
/// cancel requests through Unix are not forwarded to a backend at startup
/// time because they carry no addr to pair with the running client. We still
/// consume the startup bytes so the client reads our ErrorResponse instead
/// of seeing a bare EOF — the symptom the operator saw in the max_connections
/// regression.
pub async fn client_entrypoint_too_many_clients_already_unix(
    mut stream: UnixStream,
    connection_id: u64,
) -> Result<(), Error> {
    crate::web::metrics::record_listener_rejection("too_many_clients");
    match get_startup_with_pre_auth_timeout::<UnixStream>(&mut stream).await {
        Ok((ClientConnectionType::Tls, _)) => {
            // Unix sockets never negotiate TLS; mirror the main Unix entrypoint
            // and refuse the SSL request with the same error message.
            error_response_terminal(
                &mut stream,
                "TLS is not supported on Unix socket connections",
                "08P01",
            )
            .await?;
            return Ok(());
        }
        Ok((ClientConnectionType::Startup, _)) => (),
        Ok((ClientConnectionType::CancelQuery, _)) => {
            // A cancel request arriving while the server is at capacity is a
            // no-op: we have no worker slot to forward it to. Report back the
            // same "too many clients" error so the client sees a structured
            // response instead of EOF.
        }
        Err(err) => {
            warn!("[#c{connection_id}] unix client bad startup: {err}");
            return Err(err);
        }
    }
    error_response_terminal(&mut stream, "sorry, too many clients already", "53300").await?;
    Ok(())
}

/// Client entrypoint. Returns session identity on success for disconnect logging.
pub async fn client_entrypoint(
    mut stream: TcpStream,
    client_server_map: ClientServerMap,
    admin_only: bool,
    tls_acceptor: Option<tokio_native_tls::TlsAcceptor>,
    tls_rate_limiter: Option<RateLimiter>,
    connection_id: u64,
) -> Result<Option<ClientSessionInfo>, Error> {
    // Per-connection entrypoint: borrow the live Arc<Config> instead of
    // deep-cloning the whole Config to read a few general fields.
    let config = config_arc();
    let log_client_connections = config.general.log_client_connections;
    let tls_mode = config.general.tls_mode.clone();

    // Figure out if the client wants TLS or not.
    let addr = match stream.peer_addr() {
        Ok(addr) => addr,
        Err(err) => {
            return Err(Error::SocketError(format!(
                "Failed to get peer address: {err:?}"
            )));
        }
    };

    // pre-auth slowloris on plain TCP. Without
    // a timeout an unauthenticated client could open N TCP sockets,
    // complete the 3-way handshake, then never send the 4-byte
    // length field - each pins one tokio task + 1 FD until process
    // FD limit. 15 s matches libpq's default `connect_timeout` and
    // is far above the millisecond-class real startup latency, so
    // it does NOT interact with cancel-handshake reads (cancel is
    // 16 bytes in a single TCP write, completes in <<15s).
    let startup_result = get_startup_with_pre_auth_timeout::<TcpStream>(&mut stream).await;
    match startup_result {
        // Client requested a TLS connection.
        Ok((ClientConnectionType::Tls, _)) => {
            // TLS settings are configured, will setup TLS now.
            if let Some(tls_acceptor) = tls_acceptor {
                TLS_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
                write_all_flush(&mut stream, b"S").await?;

                if let Some(tls_rate_limiter) = tls_rate_limiter {
                    // gracefully fail the handshake instead of
                    // panicking if the rate-limiter task died.
                    if let Err(err) = tls_rate_limiter.wait().await {
                        log::warn!("tls rate-limiter unavailable, refusing TLS handshake: {err}");
                        return Ok(None);
                    }
                }

                // Negotiate TLS.
                match startup_tls(
                    stream,
                    client_server_map,
                    admin_only,
                    tls_acceptor,
                    connection_id,
                )
                .await
                {
                    Ok(mut client) => {
                        if log_client_connections {
                            info!(
                                "[{}@{} #c{}] client connected from {addr} (TLS)",
                                client.username, client.pool_name, client.connection_id
                            );
                        }
                        let session_info = ClientSessionInfo {
                            username: client.username.clone(),
                            pool_name: client.pool_name.clone(),
                            connection_id: client.connection_id,
                        };
                        let result = client.handle().await;
                        if !client.is_admin() && result.is_err() {
                            warn!(
                                "[{}@{} #c{}] client {} disconnected with error: {}",
                                client.username,
                                client.pool_name,
                                client.connection_id,
                                addr,
                                result.as_ref().unwrap_err()
                            );
                            client.disconnect_stats();
                        }
                        result.map(|_| Some(session_info))
                    }
                    Err(err) => Err(err),
                }
            }
            // TLS is not configured, we cannot offer it.
            else {
                // Rejecting client request for TLS.
                PLAIN_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
                write_all_flush(&mut stream, b"N").await?;

                // Attempting regular startup. Client can disconnect now
                // if they choose. Keep the same slowloris guard on the
                // post-`N` plain-TCP retry path.
                let post_n_startup =
                    get_startup_with_pre_auth_timeout::<TcpStream>(&mut stream).await;
                match post_n_startup {
                    // Client accepted unencrypted connection.
                    Ok((ClientConnectionType::Startup, bytes)) => {
                        #[cfg(unix)]
                        let raw_fd = Some(stream.as_raw_fd());
                        let (read, write) = split(stream);
                        drive_authenticated_client(
                            read,
                            write,
                            ClientTransport::Tcp {
                                peer: addr,
                                ssl: false,
                            },
                            bytes,
                            client_server_map,
                            admin_only,
                            connection_id,
                            #[cfg(unix)]
                            raw_fd,
                            #[cfg(all(unix, feature = "tls-migration"))]
                            None, // no SSL for plain TCP
                            log_client_connections,
                            "plain",
                        )
                        .await
                    }

                    // Legitimate post-SSL-reject CancelRequest. libpq cancel
                    // sockets opened with `sslmode=prefer` (the default) first
                    // send `SSLRequest`, accept the `'N'` rejection, then send
                    // `CancelRequest` over the same plain socket. Route it the
                    // same way the direct-cancel arm below does - anything
                    // else surfaces at the libpq client as
                    // `query cancellation failed: ... server closed the
                    // connection unexpectedly`. Same handler shape as the
                    // post-TLS cancel path in `startup.rs::startup_tls`.
                    Ok((ClientConnectionType::CancelQuery, bytes)) => {
                        CANCEL_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
                        configure_tcp_socket_for_cancel(&stream);
                        let (read, write) = split(stream);
                        match Client::cancel(read, write, addr, bytes, client_server_map).await {
                            Ok(mut client) => {
                                info!("Cancel request from {addr} (post-SSL-reject)");
                                let result = client.handle().await;
                                if !client.is_admin() && result.is_err() {
                                    client.disconnect_stats();
                                }
                                result.map(|_| None)
                            }
                            Err(err) => Err(err),
                        }
                    }

                    // A repeated SSLRequest after we already replied `'N'` is
                    // a malformed startup - keep treating it as a protocol
                    // error.
                    Ok((ClientConnectionType::Tls, _)) => {
                        crate::web::metrics::record_listener_rejection("protocol_error");
                        Err(Error::ProtocolSyncError(
                            "Unexpected protocol message during plain-text startup negotiation"
                                .into(),
                        ))
                    }

                    Err(err) => {
                        crate::web::metrics::record_listener_rejection("invalid_startup");
                        Err(err)
                    }
                }
            }
        }

        // Client wants to use plain connection without encryption.
        Ok((ClientConnectionType::Startup, bytes)) => {
            if tls_mode.is_some() && config.general.only_ssl_connections() {
                error_response_terminal(
                    &mut stream,
                    "Connection without SSL is not allowed by tls_mode.",
                    "28000",
                )
                .await?;
                crate::web::metrics::record_listener_rejection("tls_required");
                return Err(Error::ProtocolSyncError("ssl is required".to_string()));
            }
            PLAIN_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
            #[cfg(unix)]
            let raw_fd = Some(stream.as_raw_fd());
            let (read, write) = split(stream);
            drive_authenticated_client(
                read,
                write,
                ClientTransport::Tcp {
                    peer: addr,
                    ssl: false,
                },
                bytes,
                client_server_map,
                admin_only,
                connection_id,
                #[cfg(unix)]
                raw_fd,
                #[cfg(all(unix, feature = "tls-migration"))]
                None, // no SSL for plain TCP
                log_client_connections,
                "plain",
            )
            .await
        }

        // Client wants to cancel a query.
        Ok((ClientConnectionType::CancelQuery, bytes)) => {
            CANCEL_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
            // Important: without configuring the TCP socket for cancel requests,
            // libpq-based clients (e.g., psycopg2) may emit a noisy stderr warning on cancellation
            // such as:
            // "query cancellation failed: cancellation failed: connection to server ..."
            // We set the appropriate socket options to avoid this spurious message.
            configure_tcp_socket_for_cancel(&stream);
            let (read, write) = split(stream);

            // Continue with cancel query request.
            match Client::cancel(read, write, addr, bytes, client_server_map).await {
                Ok(mut client) => {
                    info!("Cancel request from {addr}");
                    let result = client.handle().await;
                    if !client.is_admin() && result.is_err() {
                        client.disconnect_stats();
                    }
                    result.map(|_| None)
                }

                Err(err) => Err(err),
            }
        }

        // Something failed, probably the socket.
        Err(err) => {
            crate::web::metrics::record_listener_rejection("invalid_startup");
            error!("#c{connection_id} client {addr} startup failed: {err}");
            Err(err)
        }
    }
}

/// Unix socket client entrypoint. Uses placeholder addr 127.0.0.1:0 (Unix sockets have no peer address).
pub async fn client_entrypoint_unix(
    mut stream: UnixStream,
    client_server_map: ClientServerMap,
    admin_only: bool,
    connection_id: u64,
) -> Result<Option<ClientSessionInfo>, Error> {
    // Per-connection entrypoint: borrow the live Arc<Config> instead of
    // deep-cloning the whole Config to read one general field.
    let config = config_arc();
    let log_client_connections = config.general.log_client_connections;

    match get_startup_with_pre_auth_timeout::<UnixStream>(&mut stream).await {
        Ok((ClientConnectionType::Startup, bytes)) => {
            PLAIN_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let raw_fd = Some(stream.as_raw_fd());
            let (read, write) = split(stream);
            drive_authenticated_client(
                read,
                write,
                ClientTransport::Unix,
                bytes,
                client_server_map,
                admin_only,
                connection_id,
                #[cfg(unix)]
                raw_fd,
                #[cfg(all(unix, feature = "tls-migration"))]
                None, // no SSL on Unix socket
                log_client_connections,
                "unix",
            )
            .await
        }

        Ok((ClientConnectionType::Tls, _)) => {
            error_response_terminal(
                &mut stream,
                "TLS is not supported on Unix socket connections",
                "08P01",
            )
            .await?;
            crate::web::metrics::record_listener_rejection("protocol_error");
            Err(Error::ProtocolSyncError(
                "TLS requested on Unix socket".into(),
            ))
        }

        Ok((ClientConnectionType::CancelQuery, bytes)) => {
            CANCEL_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (read, write) = split(stream);

            // Unix clients have no peer addr; use the loopback sentinel the
            // TCP path would have seen so Client::cancel can still do its
            // client_server_map lookup by process_id + secret_key.
            let sentinel_addr = std::net::SocketAddr::from((
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                0,
            ));
            match Client::cancel(read, write, sentinel_addr, bytes, client_server_map).await {
                Ok(mut client) => {
                    info!("Cancel request via unix socket");
                    let result = client.handle().await;
                    if !client.is_admin() && result.is_err() {
                        client.disconnect_stats();
                    }
                    result.map(|_| None)
                }
                Err(err) => Err(err),
            }
        }

        Err(err) => {
            crate::web::metrics::record_listener_rejection("invalid_startup");
            error!("#c{connection_id} unix client startup failed: {err}");
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{timeout, Duration};

    fn empty_client_server_map() -> ClientServerMap {
        Arc::new(DashMap::new())
    }

    async fn idle_tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr);
        let accepted = listener.accept();
        let (client, accepted) = tokio::join!(client, accepted);
        let (server, _) = accepted.unwrap();
        (client.unwrap(), server)
    }

    #[tokio::test]
    async fn too_many_tcp_rejection_times_out_idle_startup() {
        let (_client, server) = idle_tcp_pair().await;

        let result = timeout(
            Duration::from_millis(200),
            client_entrypoint_too_many_clients_already(server, empty_client_server_map()),
        )
        .await;

        assert!(
            result.is_ok(),
            "over-limit TCP rejection must not wait forever for StartupMessage"
        );
        assert!(matches!(result.unwrap(), Err(Error::ClientBadStartup)));
    }

    /// Companion to `post_ssl_reject_cancel_request_is_routed_not_bounced`,
    /// but for the `too_many_clients` overload entrypoint. beeaea7 fixed
    /// post-SSL-reject cancel routing on the NORMAL path; the overload path
    /// (`client_entrypoint_too_many_clients_already`) was never mirrored.
    ///
    /// A libpq cancel socket opened with `sslmode=prefer` (the default)
    /// first sends `SSLRequest`. When TLS is not configured pg_doorman
    /// replies `'N'` and must then read the next startup-class message -
    /// the `CancelRequest`. On the overload path the `Tls` arm instead fell
    /// straight through to `error_response_terminal(... "53300")`, sending
    /// the client an ErrorResponse (`'E'`) and dropping the cancel. A
    /// CancelRequest must be honored even at `max_client_connections`,
    /// because a cancel connection is ephemeral and occupies no pool slot.
    ///
    /// Observable distinction: when routed, `Client::cancel` no-ops on the
    /// empty `client_server_map` (`handle_cancel_mode` returns `Ok(())` on
    /// miss) and the socket is closed - the client sees EOF and NEVER an
    /// `'E'` ErrorResponse byte.
    #[tokio::test]
    async fn too_many_clients_post_ssl_reject_cancel_request_is_routed_not_rejected() {
        use crate::messages::constants::{CANCEL_REQUEST_CODE, SSL_REQUEST_CODE};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut client, server) = idle_tcp_pair().await;

        let client_task = tokio::spawn(async move {
            // 1. SSLRequest (length=8, code=SSL_REQUEST_CODE)
            let mut req = Vec::with_capacity(8);
            req.extend_from_slice(&8i32.to_be_bytes());
            req.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
            client.write_all(&req).await.expect("write SSLRequest");

            // 2. Read pg_doorman's `'N'` (TLS-not-configured rejection).
            let mut n_buf = [0u8; 1];
            client
                .read_exact(&mut n_buf)
                .await
                .expect("read 'N' for SSL rejection");
            assert_eq!(n_buf[0], b'N', "expected 'N' for TLS-not-configured");

            // 3. CancelRequest (16 bytes, fabricated pid/secret).
            let mut cancel = Vec::with_capacity(16);
            cancel.extend_from_slice(&16i32.to_be_bytes());
            cancel.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
            cancel.extend_from_slice(&1i32.to_be_bytes());
            cancel.extend_from_slice(&1i32.to_be_bytes());
            client
                .write_all(&cancel)
                .await
                .expect("write CancelRequest");
            let _ = client.flush().await;

            // 4. Drain until EOF. Collect anything the server sent. On the
            //    buggy path this is the 53300 ErrorResponse (starts with
            //    `'E'`); on the fixed path it is empty (cancel routed, sock
            //    closed).
            let mut tail = Vec::new();
            let _ = client.read_to_end(&mut tail).await;
            tail
        });

        let result = timeout(
            Duration::from_secs(2),
            client_entrypoint_too_many_clients_already(server, empty_client_server_map()),
        )
        .await;

        let tail = client_task.await.expect("client task joined");

        let outcome = result.expect("entrypoint must finish, not hang");
        assert!(
            outcome.is_ok(),
            "post-SSL CancelRequest on overload path must route cleanly, got: {outcome:?}"
        );
        assert!(
            !tail.starts_with(b"E"),
            "client must NOT receive a 53300 ErrorResponse for a CancelRequest; \
             cancel was dropped on the overload path. tail={tail:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn too_many_unix_rejection_times_out_idle_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pg_doorman.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path);
        let accepted = listener.accept();
        let (client, accepted) = tokio::join!(client, accepted);
        let _client = client.unwrap();
        let (server, _) = accepted.unwrap();

        let result = timeout(
            Duration::from_millis(200),
            client_entrypoint_too_many_clients_already_unix(server, 42),
        )
        .await;

        assert!(
            result.is_ok(),
            "over-limit Unix rejection must not wait forever for StartupMessage"
        );
        assert!(matches!(result.unwrap(), Err(Error::ClientBadStartup)));
    }

    /// Reproducer for the cancel-after-SSL-rejected bug a colleague hit on
    /// `pg_doorman:2.5.0` (=`local3.6.2`). libpq cancel sockets opened with
    /// `sslmode=prefer` (the default) first send an `SSLRequest`. When TLS
    /// is not configured pg_doorman replies `'N'` and waits for the next
    /// startup-class message. libpq then sends the `CancelRequest` over
    /// the same plain socket - a perfectly legitimate path per the
    /// PostgreSQL protocol. The current `entrypoint.rs` branch for that
    /// case throws `Err(Error::ProtocolSyncError("Unexpected protocol
    /// message during plain-text startup negotiation"))`, dropping the
    /// cancel, surfacing as
    /// `query cancellation failed: ... server closed the connection
    /// unexpectedly` on the client. The direct-cancel path (no preceding
    /// SSLRequest, line ~394) and the post-TLS cancel path
    /// (`startup.rs:411`) both route correctly. Only this seam is broken.
    #[tokio::test]
    async fn post_ssl_reject_cancel_request_is_routed_not_bounced() {
        use crate::messages::constants::{CANCEL_REQUEST_CODE, SSL_REQUEST_CODE};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut client, server) = idle_tcp_pair().await;

        let client_task = tokio::spawn(async move {
            // 1. SSLRequest (length=8, code=SSL_REQUEST_CODE)
            let mut req = Vec::with_capacity(8);
            req.extend_from_slice(&8i32.to_be_bytes());
            req.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
            client.write_all(&req).await.expect("write SSLRequest");

            // 2. Read pg_doorman's `'N'` (TLS-not-configured rejection).
            let mut n_buf = [0u8; 1];
            client
                .read_exact(&mut n_buf)
                .await
                .expect("read 'N' for SSL rejection");
            assert_eq!(n_buf[0], b'N', "expected 'N' for TLS-not-configured");

            // 3. CancelRequest (16 bytes, fabricated pid/secret).
            // The handler silently no-ops on an empty client_server_map
            // (transaction.rs `handle_cancel_mode` returns Ok(()) on miss).
            let mut cancel = Vec::with_capacity(16);
            cancel.extend_from_slice(&16i32.to_be_bytes());
            cancel.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
            cancel.extend_from_slice(&1i32.to_be_bytes());
            cancel.extend_from_slice(&1i32.to_be_bytes());
            client
                .write_all(&cancel)
                .await
                .expect("write CancelRequest");
            let _ = client.flush().await;

            // 4. Drain until EOF - server drops the cancel sock after routing.
            let mut tail = Vec::new();
            let _ = client.read_to_end(&mut tail).await;
        });

        let result = timeout(
            Duration::from_secs(2),
            client_entrypoint(server, empty_client_server_map(), false, None, None, 100),
        )
        .await;

        let _ = client_task.await;

        let outcome = result.expect("client_entrypoint must finish, not hang");
        match outcome {
            Ok(None) => {}
            Ok(Some(_)) => panic!("cancel path must not produce a ClientSessionInfo"),
            Err(err) => panic!("post-SSL CancelRequest must be routed cleanly, got error: {err}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_entrypoint_times_out_idle_startup() {
        let (client, server) = UnixStream::pair().unwrap();
        let _client = client;

        let result = timeout(
            Duration::from_millis(200),
            client_entrypoint_unix(server, empty_client_server_map(), false, 43),
        )
        .await;

        assert!(
            result.is_ok(),
            "normal Unix entrypoint must not wait forever for StartupMessage"
        );
        assert!(matches!(result.unwrap(), Err(Error::ClientBadStartup)));
    }
}
