use bytes::{Buf, BufMut, BytesMut};
use log::error;
use std::ffi::CStr;
use std::str;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{split, AsyncReadExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use crate::auth::hba::CheckResult;
use crate::auth::talos::{extract_talos_token, log_talos_routing, resolve_talos_user};
use crate::auth::{authenticate, OperatorManagedKeys};
#[cfg(test)]
use crate::config::check_hba_with_general;
use crate::config::{check_hba, config_arc, get_config};
use crate::errors::{ClientIdentifier, Error};
use crate::messages::constants::*;
use crate::messages::{
    error_response_terminal, parse_startup, plain_password_challenge, read_password,
    ready_for_query, write_all_flush,
};
use crate::pool::ClientServerMap;
use crate::server::ServerParameters;
use crate::stats::{ClientStats, CANCEL_CONNECTION_COUNTER};
use crate::transport::ClientTransport;

use super::buffer_pool::PooledBuffer;
use super::core::{Client, PreparedStatementState};

/// Type of connection received from client.
pub(crate) enum ClientConnectionType {
    Startup,
    Tls,
    CancelQuery,
}

pub(crate) const PRE_AUTH_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn is_admin_database(pool_name: &str) -> bool {
    matches!(pool_name, "pgdoorman" | "pgbouncer")
}

fn talos_hba_allowed(md5: CheckResult, scram: CheckResult) -> bool {
    matches!(md5, CheckResult::Allow | CheckResult::Trust)
        || matches!(scram, CheckResult::Allow | CheckResult::Trust)
}

/// Evaluate pg_hba for the user a Talos token resolved to. The token itself
/// authenticates the literal `talos` user, but the session is then routed to a
/// derived user (personal pool, service pool, or role); without this check a
/// `reject` rule written for that user would never apply.
fn talos_resolved_hba_checks(
    transport: &ClientTransport,
    username: &str,
    pool_name: &str,
) -> (CheckResult, CheckResult) {
    (
        check_hba(transport, "md5", username, pool_name),
        check_hba(transport, "scram-sha-256", username, pool_name),
    )
}

#[cfg(test)]
fn talos_resolved_hba_allowed_with_general(
    general: &crate::config::General,
    transport: &ClientTransport,
    username: &str,
    pool_name: &str,
) -> bool {
    let md5 = check_hba_with_general(general, transport, "md5", username, pool_name);
    let scram = check_hba_with_general(general, transport, "scram-sha-256", username, pool_name);
    talos_hba_allowed(md5, scram)
}

fn merge_safe_client_startup_parameters(
    server_parameters: &mut ServerParameters,
    parameters: &std::collections::HashMap<String, String>,
    operator_managed_keys: Option<&OperatorManagedKeys>,
    talos_application_name: Option<&str>,
) {
    for (key, value) in parameters {
        if !crate::server::parameters::is_safe_client_startup_key(key) {
            continue;
        }
        if let Some(keys) = operator_managed_keys {
            let canonical = crate::server::parameters::canonicalize_param_name(key.clone());
            if keys.contains(&canonical) {
                continue;
            }
        }
        let _ = server_parameters.set_param(key.clone(), value.clone(), true);
    }

    if let Some(application_name) = talos_application_name {
        let operator_managed_application_name =
            operator_managed_keys.is_some_and(|keys| keys.contains("application_name"));
        if !operator_managed_application_name {
            let _ = server_parameters.set_param("application_name", application_name, true);
        }
    }
}

/// Handle the first message the client sends.
pub(crate) async fn get_startup<S>(
    stream: &mut S,
) -> Result<(ClientConnectionType, BytesMut), Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin + tokio::io::AsyncWrite,
{
    // Get startup message length.
    let len = match stream.read_i32().await {
        Ok(len) => len,
        Err(_) => return Err(Error::ClientBadStartup),
    };

    // Validate message length: minimum is 8 bytes (4 for length field + 4 for protocol code).
    // Also reject negative or excessively large lengths to prevent overflow/DoS.
    if !(8..=8 * 1024).contains(&len) {
        return Err(Error::ClientBadStartup);
    }

    // Get the rest of the message.
    let mut startup = vec![0u8; (len - 4) as usize];
    match stream.read_exact(&mut startup).await {
        Ok(_) => (),
        Err(_) => return Err(Error::ClientBadStartup),
    };

    let mut bytes = BytesMut::from(&startup[..]);
    let code = bytes.get_i32();

    match code {
        // Client is requesting SSL (TLS).
        SSL_REQUEST_CODE => Ok((ClientConnectionType::Tls, bytes)),

        // Client wants to use plain text, requesting regular startup.
        PROTOCOL_VERSION_NUMBER => Ok((ClientConnectionType::Startup, bytes)),

        // PG 18+ clients (libpq 18) negotiate protocol 3.2
        // (196610) by default. Without explicit handling pg_doorman
        // rejected the connection with "Unexpected startup code" - any
        // operator upgrading libpq to 18 lost ALL TCP connections from
        // that client. PG protocol spec says the server (or proxy) must
        // either accept the higher minor and continue OR send
        // NegotiateProtocolVersion to downgrade. We pick the former:
        // major version 3 with any minor is treated as protocol 3.0 -
        // pg_doorman only consumes the StartupMessage parameters and
        // forwards to the backend, which performs its own renegotiation
        // if it speaks an older protocol than the client. This is the
        // simplest path that preserves PG 18 client compatibility.
        // Code layout: high 16 bits = major, low 16 bits = minor.
        code if (code >> 16) == 3 => Ok((ClientConnectionType::Startup, bytes)),

        // Client is requesting to cancel a running query (plain text connection).
        CANCEL_REQUEST_CODE => Ok((ClientConnectionType::CancelQuery, bytes)),

        REQUEST_GSSENCMODE_CODE => {
            // Reject GSSENCMODE the same way `postmaster.c` does when GSSAPI
            // is unavailable: write `'N'` (no-GSS, fall back) and let the
            // client retry startup on the SAME socket - libpq's
            // `fe-connect.c::CONNECTION_GSS_STARTUP` accepts `'N'` as the
            // signal to send SSLRequest or StartupMessage next. The legacy
            // `'G'` byte here told libpq to begin a GSSAPI handshake, and
            // the subsequent `Err(AuthError)` dropped the socket - every
            // Linux client with a Kerberos ticket cache (psql/psycopg/
            // pg_dump on RHEL/Ubuntu/Debian, where libpq is built with
            // `ENABLE_GSS=1` and `gssencmode=prefer` is the default) then
            // saw "server closed the connection unexpectedly". Same
            // beeaea7-shaped state-machine drop as the post-SSL-reject arm.
            let no = [b'N'];
            write_all_flush(stream, &no).await?;
            // Async recursion needs `Box::pin`. The outer
            // `get_startup_with_pre_auth_timeout` wraps the whole future,
            // so the second read still inherits the 15s slowloris bound.
            Box::pin(get_startup(stream)).await
        }

        // Something else, probably something is wrong, and it's not our fault,
        // e.g. badly implemented Postgres client.
        _ => Err(Error::ProtocolSyncError(format!(
            "Unexpected startup code: {code}"
        ))),
    }
}

/// Handle TLS connection negotiation.
pub async fn startup_tls(
    stream: TcpStream,
    client_server_map: ClientServerMap,
    admin_only: bool,
    tls_acceptor: tokio_native_tls::TlsAcceptor,
    connection_id: u64,
) -> Result<
    Client<
        ReadHalf<tokio_native_tls::TlsStream<TcpStream>>,
        WriteHalf<tokio_native_tls::TlsStream<TcpStream>>,
    >,
    Error,
> {
    // Negotiate TLS.
    let addr = match stream.peer_addr() {
        Ok(addr) => addr,
        Err(err) => {
            return Err(Error::SocketError(format!(
                "Failed to get peer address: {err:?}"
            )));
        }
    };

    // Capture TCP fd before TLS wrapping — needed for migration
    #[cfg(unix)]
    let tcp_raw_fd = {
        use std::os::unix::io::AsRawFd;
        stream.as_raw_fd()
    };

    // pre-auth slowloris DoS. Without a timeout
    // around `tls_acceptor.accept`, an unauthenticated TCP client
    // could open N connections, send `SSLRequest` (8 bytes), then
    // drip ClientHello bytes at 1 byte/min - each connection pins
    // one tokio task + one FD until the OS keepalive (minutes-to-
    // hours) fires. With pre-auth slots ≈ process FD limit, the
    // listener stops accepting legitimate clients within seconds.
    // 15-second cap matches libpq's default `connect_timeout`.
    const PRE_AUTH_TLS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    let tls_result = tokio::time::timeout(PRE_AUTH_TLS_TIMEOUT, tls_acceptor.accept(stream)).await;
    let mut stream = match tls_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            crate::web::metrics::record_listener_rejection("tls_handshake_fail");
            error!("TLS negotiation failed: {err}");
            return Err(Error::TlsError);
        }
        Err(_) => {
            crate::web::metrics::record_listener_rejection("tls_handshake_timeout");
            error!(
                "TLS negotiation timed out after {}s - possible slowloris",
                PRE_AUTH_TLS_TIMEOUT.as_secs()
            );
            return Err(Error::TlsError);
        }
    };

    // TLS negotiation successful.
    // Continue with regular startup using encrypted connection.
    // same slowloris guard on the post-TLS
    // startup read. PRE_AUTH_TLS_TIMEOUT (15s) is a generous cap
    // that does not interact with cancel-handshake reads.
    let post_tls_result = tokio::time::timeout(
        PRE_AUTH_TLS_TIMEOUT,
        get_startup::<tokio_native_tls::TlsStream<TcpStream>>(&mut stream),
    )
    .await;
    let post_tls_startup = match post_tls_result {
        Ok(r) => r,
        Err(_) => {
            crate::web::metrics::record_listener_rejection("post_tls_startup_timeout");
            return Err(Error::ClientBadStartup);
        }
    };
    match post_tls_startup {
        // Got good startup message, proceeding like normal except we
        // are encrypted now.
        Ok((ClientConnectionType::Startup, bytes)) => {
            #[cfg(unix)]
            let raw_fd = Some(tcp_raw_fd);
            // SSL* pointer for TLS migration — only with vendored patched OpenSSL on Linux
            #[cfg(all(target_os = "linux", feature = "tls-migration"))]
            let ssl_ptr = Some(crate::client::core::SslRawPtr(
                stream.get_ref().ssl_raw_ptr(),
            ));
            #[cfg(all(unix, feature = "tls-migration", not(target_os = "linux")))]
            let ssl_ptr = None;
            let (read, write) = split(stream);

            startup_with_auth_timeout(
                read,
                write,
                ClientTransport::Tcp {
                    peer: addr,
                    ssl: true,
                },
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
        }

        Ok((ClientConnectionType::CancelQuery, bytes)) => {
            CANCEL_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
            // peel the wrapped layers to reach the raw
            // `TcpStream` so we can apply the same SO_LINGER/TCP_NODELAY
            // tuning that `client_entrypoint`'s direct-cancel and
            // post-SSL-reject arms apply via
            // `configure_tcp_socket_for_cancel`. Without it libpq cancel
            // sockets opened with `sslmode=require` (psycopg3
            // `cancel_safe()` reuses the parent connection's sslmode per
            // libpq 17 `PQcancelConn` semantics) print a noisy
            // `cancellation failed: ... server closed the connection
            // unexpectedly` warning to stderr after every successful
            // cancel - the exact behaviour the helper exists to suppress.
            // `tokio_native_tls::TlsStream::get_ref()` returns the
            // underlying `native_tls::TlsStream`; its own `.get_ref()`
            // returns the wrapped `TcpStream`.
            crate::messages::config_socket::configure_tcp_socket_for_cancel(
                stream.get_ref().get_ref().get_ref(),
            );
            let (read, write) = split(stream);
            Client::cancel(read, write, addr, bytes, client_server_map).await
        }

        Ok((ClientConnectionType::Tls, _)) => {
            crate::web::metrics::record_listener_rejection("protocol_error");
            Err(Error::ProtocolSyncError("Bad postgres client (tls)".into()))
        }

        Err(err) => {
            crate::web::metrics::record_listener_rejection("invalid_startup");
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn startup_with_auth_timeout<S, T>(
    read: S,
    write: T,
    transport: ClientTransport,
    bytes: BytesMut,
    client_server_map: ClientServerMap,
    admin_only: bool,
    connection_id: u64,
    #[cfg(unix)] raw_fd: Option<std::os::unix::io::RawFd>,
    #[cfg(all(unix, feature = "tls-migration"))] ssl_ptr: Option<super::core::SslRawPtr>,
    timeout_duration: std::time::Duration,
) -> Result<Client<S, T>, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    let startup = async {
        let _runtime_dependency_guard =
            crate::config::runtime_dependency_publish_read_guard().await;
        Client::startup(
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
        )
        .await
    };

    match tokio::time::timeout(timeout_duration, startup).await {
        Ok(result) => result,
        Err(_) => {
            crate::web::metrics::record_listener_rejection("auth_timeout");
            Err(Error::ClientBadStartup)
        }
    }
}

impl<S, T> Client<S, T>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    /// Handle Postgres client startup after TLS negotiation is complete
    /// or over plain text.
    #[allow(clippy::too_many_arguments)]
    pub async fn startup(
        mut read: S,
        mut write: T,
        transport: ClientTransport,
        bytes: BytesMut, // The rest of the startup message.
        client_server_map: ClientServerMap,
        admin_only: bool,
        connection_id: u64,
        #[cfg(unix)] raw_fd: Option<std::os::unix::io::RawFd>,
        #[cfg(all(unix, feature = "tls-migration"))] ssl_ptr: Option<super::core::SslRawPtr>,
    ) -> Result<Client<S, T>, Error> {
        // Unix sockets have no peer address; we pin a sentinel loopback
        // value into the Client struct so the many transaction-level log
        // lines that interpolate `self.addr` keep compiling. A follow-up
        // refactor should replace this with a typed PeerAddress field.
        let addr = match transport {
            ClientTransport::Tcp { peer, .. } => peer,
            ClientTransport::Unix => {
                std::net::SocketAddr::from((std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0))
            }
        };
        let use_tls = transport.is_tls();
        let parameters = parse_startup(bytes)?;

        // This parameter is mandatory by the protocol.
        let username_from_parameters = match parameters.get("user") {
            Some(user) => user,
            None => {
                return Err(Error::ClientError(
                    "Missing 'user' parameter in connection string. Please specify a username in your connection string.".into(),
                ))
            }
        };

        let pool_name = parameters
            .get("database")
            .unwrap_or(username_from_parameters)
            .to_string();
        let admin = is_admin_database(&pool_name);

        let application_name = match parameters.get("application_name") {
            Some(application_name) => application_name,
            None => "pg_doorman",
        };

        let mut client_identifier = ClientIdentifier::new(
            application_name,
            username_from_parameters,
            &pool_name,
            transport.peer_display().as_str(),
        );
        client_identifier.use_tls = transport.is_tls();
        client_identifier.hba_md5 =
            check_hba(&transport, "md5", username_from_parameters, &pool_name);
        client_identifier.hba_scram = check_hba(
            &transport,
            "scram-sha-256",
            username_from_parameters,
            &pool_name,
        );
        {
            // If md5 or scram is allowed, we can try to authenticate with Talos.
            let hba_ok = client_identifier.hba_md5 == CheckResult::Allow
                || client_identifier.hba_scram == CheckResult::Allow;
            if !admin && username_from_parameters == TALOS_USERNAME && hba_ok {
                plain_password_challenge(&mut write).await?;
                let talos_token_response = read_password(&mut read).await?;
                let talos_token_with_nul = match str::from_utf8(&talos_token_response) {
                    Ok(token) => token,
                    Err(_) => {
                        error_response_terminal(
                            &mut write,
                            "Invalid Talos token format. Token must be valid UTF-8 text.",
                            "3D000",
                        )
                        .await?;
                        return Err(Error::AuthError(format!(
                            "Failed to parse Talos token as UTF-8 for user: {TALOS_USERNAME}"
                        )));
                    }
                };
                let talos_token = match CStr::from_bytes_until_nul(talos_token_with_nul.as_ref()) {
                    Ok(token) => match token.to_str() {
                        Ok(s) => s.to_string(),
                        Err(_) => {
                            error_response_terminal(
                                &mut write,
                                "Invalid Talos token: contains non-UTF-8 bytes.",
                                "28000",
                            )
                            .await?;
                            return Err(Error::AuthError(
                                "Talos token contains non-UTF-8 bytes".to_string(),
                            ));
                        }
                    },
                    Err(_) => {
                        error_response_terminal(
                            &mut write,
                            "Invalid Talos token format. Token must be a null-terminated string.",
                            "3D000",
                        )
                        .await?;
                        return Err(Error::AuthError(format!(
                            "Failed to convert Talos token to string for user: {TALOS_USERNAME}. Token must be null-terminated."
                        )));
                    }
                };
                // Per-connection: borrow the live Arc<Config> and clone only
                // the Talos fields instead of deep-cloning the whole Config.
                let config = config_arc();
                let talos_databases = config.talos.databases.clone();
                let talos_resource_prefixes = config.talos.resource_prefixes.clone();
                let token = match extract_talos_token(
                    talos_token,
                    &pool_name,
                    talos_databases,
                    talos_resource_prefixes,
                )
                .await
                {
                    Ok(token) => token,
                    Err(err) => {
                        error!("Invalid Talos token for database {pool_name}: {err:?}");
                        error_response_terminal(&mut write, "Invalid Talos token.", "3D000")
                            .await?;
                        return Err(Error::AuthError(format!("Invalid Talos token: {err:?}")));
                    }
                };
                let resolved = resolve_talos_user(
                    pool_name.as_str(),
                    &token.client_id,
                    token.role,
                    crate::pool::pool_exists,
                );
                log_talos_routing(&token.client_id, pool_name.as_str(), token.role, &resolved);
                // The token authenticated the literal `talos` user; gate the
                // user it resolved to as well, so a pg_hba rule written for a
                // personal/service pool or a role still applies.
                let (resolved_hba_md5, resolved_hba_scram) =
                    talos_resolved_hba_checks(&transport, &resolved.username, &pool_name);
                if !talos_hba_allowed(resolved_hba_md5, resolved_hba_scram) {
                    error_response_terminal(
                        &mut write,
                        "Talos user is not allowed by pg_hba rules.",
                        "28000",
                    )
                    .await?;
                    return Err(Error::AuthError(format!(
                        "Talos user {} is not allowed by pg_hba for database {pool_name}",
                        resolved.username
                    )));
                }
                client_identifier.application_name = token.client_id.clone();
                client_identifier.username = resolved.username;
                client_identifier.hba_md5 = resolved_hba_md5;
                client_identifier.hba_scram = resolved_hba_scram;
                client_identifier.is_talos = true;
            }
        }

        // Kick any client that's not admin while we're in admin-only mode.
        if !admin && admin_only {
            error_response_terminal(
                &mut write,
                "is admin only mode: pooler is shut down now",
                "58006",
            )
            .await?;
            return Err(Error::ShuttingDown);
        }

        // Final HBA decision: if neither md5 nor scram is explicitly allowed or trusted,
        // the connection is not permitted by HBA. `Deny` indicates explicit `reject` rule,
        // while `NotMatched` means no rule matched.
        let hba_ok_final = matches!(
            client_identifier.hba_scram,
            CheckResult::Allow | CheckResult::Trust
        ) || matches!(
            client_identifier.hba_md5,
            CheckResult::Allow | CheckResult::Trust
        );
        if !hba_ok_final {
            error_response_terminal(
                &mut write,
                format!("Connection from {} to {}@{} (TLS: {}) is not permitted by HBA configuration. Please contact your database administrator.",
                        transport.peer_display(), username_from_parameters, pool_name, use_tls).as_str(),
                "28000"
            )
                .await?;
            crate::web::metrics::record_listener_rejection("hba");
            return Err(Error::HbaForbiddenError(format!(
                "Connection not permitted by HBA configuration for client: {} from {}",
                client_identifier,
                transport.peer_display()
            )));
        }

        // Derive process_id for Cancel Protocol from monotonic connection_id.
        // Wrapping is intentional: PostgreSQL uses 32-bit PIDs with the same
        // wrapping behavior. Sequential values give fewer collisions than random
        // at <50K concurrent clients. The random secret_key (below) provides
        // collision resistance after wrap-around (~2^31 connections).
        let process_id: i32 = connection_id as i32;
        let secret_key: i32 = rand::random();

        // Authenticate user
        let auth_outcome = authenticate(
            &mut read,
            &mut write,
            admin,
            &mut client_identifier,
            &pool_name,
            username_from_parameters,
        )
        .await?;
        let transaction_mode = auth_outcome.transaction_mode;
        let mut server_parameters = auth_outcome.server_parameters;
        let prepared_statements_enabled = auth_outcome.prepared_statements_enabled;
        let pool_user = auth_outcome.pool_user;
        let operator_managed_keys = auth_outcome.operator_managed_keys;
        let migration_pool = auth_outcome.pool_generation;
        let migration_pool_is_dynamic = auth_outcome.pool_generation_is_dynamic;

        // Merge safe client StartupMessage parameters into the client
        // snapshot. Configured startup_parameters win, because
        // the backend will run with those values. `startup = true`
        // keeps non-ParameterStatus GUCs such as search_path and role
        // available for checkout sync.
        let talos_application_name = client_identifier
            .is_talos
            .then_some(client_identifier.application_name.as_str());
        merge_safe_client_startup_parameters(
            &mut server_parameters,
            &parameters,
            operator_managed_keys.as_ref(),
            talos_application_name,
        );
        let mut buf = BytesMut::new();
        {
            let mut auth_ok = BytesMut::with_capacity(9);
            auth_ok.put_u8(b'R');
            auth_ok.put_i32(8);
            auth_ok.put_i32(0);
            buf.put(auth_ok);
            let server_params_buf: BytesMut = (&server_parameters).into();
            buf.put(server_params_buf);
            let mut key_data = BytesMut::from(&b"K"[..]);
            key_data.put_i32(12);
            key_data.put_i32(process_id);
            key_data.put_i32(secret_key);
            buf.put(key_data);
            buf.put(ready_for_query(false));
        }
        write_all_flush(&mut write, &buf).await?;

        let stats = Arc::new(ClientStats::new_with_pool_user(
            connection_id,
            client_identifier.application_name.as_str(),
            client_identifier.username.as_str(),
            &pool_name,
            &pool_user,
            addr.to_string().as_str(),
            crate::utils::clock::now(),
            use_tls,
        ));

        let config = get_config();
        let anon_cache_size =
            crate::pool::resolve_client_anon_cache_size(&pool_name, &config.general);
        // build the cached PoolIdentifier once at login time so
        // every per-checkout `Client::get_pool()` skips two String
        // allocations.
        let username = std::mem::take(&mut client_identifier.username);
        let cached_pool_id = crate::pool::PoolIdentifier::new(&pool_name, &pool_user);
        Ok(Client {
            // 64 KiB BufReader matches the backend-side
            // BufStream capacity so client->server pipelined batches
            // refill less often (5.85% CPU win on bulk reads).
            read: BufReader::with_capacity(crate::server::BUF_STREAM_CAPACITY, read),
            write,
            addr_str: addr.to_string(),
            addr,
            read_buf: BytesMut::with_capacity(8192),
            buffer: PooledBuffer::new(),
            cancel_mode: false,
            transaction_mode,
            sql_prepare_session_pinned: false,
            connection_id,
            secret_key,
            client_server_map,
            stats,
            admin,
            last_server_stats: None,
            connected_to_server: false,
            session_xact_start: None,
            pool_name,
            username,
            cached_pool_id,
            migration_pool,
            migration_pool_is_dynamic,
            server_parameters,
            prepared: PreparedStatementState::new(prepared_statements_enabled, anon_cache_size),
            client_last_messages_in_tx: PooledBuffer::new(),
            max_memory_usage: config.general.max_memory_usage.as_bytes(),
            client_pending_begin: None,
            pending_app_name_set: None,
            #[cfg(unix)]
            raw_fd,
            #[cfg(all(unix, feature = "tls-migration"))]
            ssl_ptr,
        })
    }

    /// Handle cancel request.
    pub async fn cancel(
        read: S,
        write: T,
        addr: std::net::SocketAddr,
        mut bytes: BytesMut, // The rest of the startup message.
        client_server_map: ClientServerMap,
    ) -> Result<Client<S, T>, Error> {
        // validate remaining bytes before consuming. An unauthenticated
        // client controls the CancelRequest body size. The caller has already
        // stripped the length prefix and the cancel protocol code, so a
        // well-formed CancelRequest must still carry 8 bytes (pid + secret).
        // `bytes::Buf::get_i32` panics on "advance past end of buffer".
        if bytes.remaining() < 8 {
            return Err(Error::ProtocolSyncError(
                "CancelRequest truncated: missing process_id/secret_key".to_string(),
            ));
        }
        let target_process_id = bytes.get_i32();
        let target_secret_key = bytes.get_i32();
        // In cancel mode, connection_id stores the target's process_id for lookup.
        Ok(Client {
            // 64 KiB BufReader matches the backend-side
            // BufStream capacity so client->server pipelined batches
            // refill less often (5.85% CPU win on bulk reads).
            read: BufReader::with_capacity(crate::server::BUF_STREAM_CAPACITY, read),
            write,
            addr_str: addr.to_string(),
            addr,
            read_buf: BytesMut::with_capacity(8192),
            connection_id: target_process_id as u64,
            buffer: PooledBuffer::new(),
            cancel_mode: true,
            transaction_mode: false,
            sql_prepare_session_pinned: false,
            secret_key: target_secret_key,
            client_server_map,
            stats: Arc::new(ClientStats::default()),
            admin: false,
            last_server_stats: None,
            pool_name: String::from("undefined"),
            username: String::from("undefined"),
            // cancel mode never calls `Client::get_pool()` - it
            // routes via client_server_map. A default identifier is the
            // honest placeholder.
            cached_pool_id: crate::pool::PoolIdentifier::default(),
            migration_pool: None,
            migration_pool_is_dynamic: false,
            server_parameters: ServerParameters::new(),
            prepared: PreparedStatementState::default(),
            connected_to_server: false,
            session_xact_start: None,
            client_last_messages_in_tx: PooledBuffer::new(),
            max_memory_usage: 128 * 1024 * 1024,
            client_pending_begin: None,
            pending_app_name_set: None,
            #[cfg(unix)]
            raw_fd: None,
            #[cfg(all(unix, feature = "tls-migration"))]
            ssl_ptr: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::is_admin_database;
    use super::merge_safe_client_startup_parameters;
    use super::startup_with_auth_timeout;
    use super::talos_resolved_hba_allowed_with_general;
    use super::{Client, ClientServerMap};
    use crate::auth::hba::PgHba;
    use crate::config::General;
    use crate::transport::ClientTransport;
    use bytes::{BufMut, BytesMut};
    use dashmap::DashMap;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    // A Talos token authenticates the literal `talos` user, but the pooler then
    // routes the session to a user derived from the token (personal pool,
    // service pool, or role). pg_hba must gate that derived user too, otherwise
    // a `reject` rule for it is silently bypassed.
    #[test]
    fn talos_resolved_user_is_checked_against_pg_hba() {
        let general = General {
            pg_hba: Some(PgHba::from_content(
                "host all talos 127.0.0.1/32 md5\n\
                 host all owner 127.0.0.1/32 reject\n\
                 host all srv-billing 127.0.0.1/32 md5",
            )),
            ..General::default()
        };
        let peer = "127.0.0.1:54321".parse().unwrap();
        let transport = ClientTransport::Tcp { peer, ssl: false };

        assert!(
            !talos_resolved_hba_allowed_with_general(&general, &transport, "owner", "db"),
            "a derived Talos user rejected by pg_hba must not be admitted"
        );
        assert!(
            talos_resolved_hba_allowed_with_general(&general, &transport, "srv-billing", "db"),
            "a derived Talos user allowed by pg_hba must stay admitted"
        );
    }

    #[test]
    fn admin_database_detection_matches_reserved_names() {
        assert!(is_admin_database("pgdoorman"));
        assert!(is_admin_database("pgbouncer"));
        assert!(!is_admin_database("example_db"));
    }

    #[test]
    fn talos_startup_merge_uses_token_application_name_for_backend() {
        let mut server_parameters = crate::server::ServerParameters::new();
        let parameters = std::collections::HashMap::from([
            (
                "application_name".to_string(),
                "spoofed-client-app".to_string(),
            ),
            ("search_path".to_string(), "public".to_string()),
        ]);

        merge_safe_client_startup_parameters(
            &mut server_parameters,
            &parameters,
            None,
            Some("talos-client-id"),
        );

        assert_eq!(server_parameters.get_application_name(), "talos-client-id");
        assert_eq!(
            server_parameters.as_hashmap().get("search_path"),
            Some(&"public".to_string())
        );
    }

    #[test]
    fn startup_routes_talos_application_name_through_merge_helper() {
        let src = include_str!("startup.rs");
        let src = src.split("\n#[cfg(test)]\nmod tests").next().unwrap_or(src);
        let startup_start = src
            .find("pub async fn startup(")
            .expect("startup function should exist");
        let startup_src = &src[startup_start..];
        let merge_idx = startup_src
            .find("merge_safe_client_startup_parameters(")
            .expect("startup must merge client startup parameters through the guarded helper");
        let auth_ok_idx = startup_src
            .find("let mut buf = BytesMut::new();")
            .expect("StartupMessage response construction should follow parameter merge");
        let merge_block = &startup_src[..auth_ok_idx];

        assert!(
            merge_block.contains("client_identifier.is_talos")
                && merge_block.contains("client_identifier.application_name.as_str()"),
            "Talos startup must pass token client_id application_name into backend parameter merge"
        );
        assert!(
            startup_src[merge_idx..auth_ok_idx].contains("talos_application_name"),
            "startup merge helper must receive the Talos application_name override"
        );
    }

    #[test]
    fn talos_branch_is_not_used_for_admin_databases() {
        let src = include_str!("startup.rs");
        let startup_idx = src
            .find("pub async fn startup")
            .expect("startup function not found");
        let impl_src = &src[startup_idx..];

        let admin_idx = impl_src
            .find("let admin = is_admin_database(&pool_name);")
            .expect("startup must classify admin databases before auth branches");
        let talos_idx = impl_src
            .find("username_from_parameters == TALOS_USERNAME")
            .expect("Talos branch not found");
        assert!(
            admin_idx < talos_idx,
            "admin database classification must run before Talos can rewrite auth identity"
        );
        assert!(
            impl_src.contains("if !admin && username_from_parameters == TALOS_USERNAME && hba_ok"),
            "Talos must be skipped for pgdoorman/pgbouncer admin databases"
        );
    }

    fn startup_params(user: &str, database: &str) -> BytesMut {
        let mut bytes = BytesMut::new();
        bytes.put_slice(b"user\0");
        bytes.put_slice(user.as_bytes());
        bytes.put_u8(0);
        bytes.put_slice(b"database\0");
        bytes.put_slice(database.as_bytes());
        bytes.put_u8(0);
        bytes.put_u8(0);
        bytes
    }

    fn password_message(password: &str) -> Vec<u8> {
        let mut bytes = BytesMut::new();
        bytes.put_u8(b'p');
        bytes.put_i32((password.len() + 1 + 4) as i32);
        bytes.put_slice(password.as_bytes());
        bytes.put_u8(0);
        bytes.to_vec()
    }

    #[tokio::test]
    async fn talos_validation_error_sent_to_client_is_sanitized() {
        let read = std::io::Cursor::new(password_message("not-a-jwt"));
        let (write, mut peer) = tokio::io::duplex(2048);
        let transport = ClientTransport::Tcp {
            peer: "127.0.0.1:54321".parse().unwrap(),
            ssl: false,
        };
        let client_server_map: ClientServerMap = Arc::new(DashMap::new());

        let err = match Client::startup(
            read,
            write,
            transport,
            startup_params(crate::messages::constants::TALOS_USERNAME, "db"),
            client_server_map,
            false,
            1,
            #[cfg(unix)]
            None,
            #[cfg(all(unix, feature = "tls-migration"))]
            None,
        )
        .await
        {
            Ok(_) => panic!("invalid Talos token must fail authentication"),
            Err(err) => err,
        };
        assert!(matches!(err, crate::errors::Error::AuthError(_)));

        let mut sent = Vec::new();
        peer.read_to_end(&mut sent).await.unwrap();
        let sent = String::from_utf8_lossy(&sent);

        assert!(sent.contains("Invalid Talos token."));
        assert!(
            !sent.contains("JWT token")
                && !sent.contains("JWTValidate")
                && !sent.contains("not enabled")
                && !sent.contains("public key"),
            "Talos validation details must stay out of unauthenticated client errors: {sent}"
        );
    }

    #[tokio::test]
    async fn startup_timeout_covers_talos_password_read() {
        let (_client_side, server_side) = tokio::io::duplex(128);
        let (read, write) = tokio::io::split(server_side);
        let transport = ClientTransport::Tcp {
            peer: "127.0.0.1:54321".parse().unwrap(),
            ssl: false,
        };
        let client_server_map: ClientServerMap = Arc::new(DashMap::new());

        let result = startup_with_auth_timeout(
            read,
            write,
            transport,
            startup_params(crate::messages::constants::TALOS_USERNAME, "db"),
            client_server_map,
            false,
            1,
            #[cfg(unix)]
            None,
            #[cfg(all(unix, feature = "tls-migration"))]
            None,
            std::time::Duration::from_millis(20),
        )
        .await;

        let err = match result {
            Ok(_) => panic!("auth timeout must abort a client that never sends PasswordMessage"),
            Err(err) => err,
        };

        assert!(matches!(err, crate::errors::Error::ClientBadStartup));
    }

    #[test]
    fn startup_waits_for_runtime_dependency_publish_barrier() {
        let src = include_str!("startup.rs");
        let start = src
            .find("#[allow(clippy::too_many_arguments)]\npub(crate) async fn startup_with_auth_timeout")
            .expect("startup_with_auth_timeout must exist");
        let end = src[start..]
            .find("\nimpl<S, T> Client")
            .map(|offset| start + offset)
            .expect("Client impl marker must exist after startup_with_auth_timeout");
        let block = &src[start..end];

        let startup_future = block
            .find("let startup = async")
            .expect("startup path must wrap auth startup in a future");
        let guard = block
            .find("runtime_dependency_publish_read_guard().await")
            .expect("startup must wait for runtime dependency publish barrier");
        let startup = block
            .find("Client::startup")
            .expect("startup path must invoke Client::startup");
        let timeout = block
            .find("tokio::time::timeout(timeout_duration, startup)")
            .expect("startup future must be enforced by auth timeout");

        assert!(
            startup_future < guard && guard < startup && startup < timeout,
            "runtime dependency publish barrier must be inside the auth timeout before Client::startup"
        );
    }

    #[test]
    fn startup_routes_via_auth_outcome_pool_user_without_relabeling_client() {
        let src = include_str!("startup.rs");
        let src = src.split("\n#[cfg(test)]\nmod tests").next().unwrap_or(src);
        let startup_start = src
            .find("pub async fn startup(")
            .expect("startup function should exist");
        let startup_src = &src[startup_start..];
        let auth_idx = startup_src
            .find("let auth_outcome = authenticate(")
            .expect("startup auth call not found");
        let stats_idx = startup_src
            .find("let stats = Arc::new(ClientStats::new_with_pool_user(")
            .expect("ClientStats construction not found");
        let cached_idx = startup_src
            .find("let cached_pool_id = crate::pool::PoolIdentifier::new")
            .expect("cached pool id construction not found");
        assert!(
            auth_idx < stats_idx && stats_idx < cached_idx,
            "startup should build client stats before cached route pool id"
        );

        let stats_block = &startup_src[stats_idx..cached_idx];
        assert!(
            stats_block.contains("client_identifier.username.as_str()"),
            "ClientStats must use the authenticated client username"
        );

        let cached_block = &startup_src[cached_idx..];
        assert!(
            cached_block.contains("&pool_user"),
            "cached PoolIdentifier must use the backend route user returned by auth"
        );
    }

    #[test]
    fn startup_uses_authenticated_pool_generation_from_auth_outcome() {
        let src = include_str!("startup.rs");
        let src = src.split("\n#[cfg(test)]\nmod tests").next().unwrap_or(src);
        let startup_start = src
            .find("pub async fn startup(")
            .expect("startup function should exist");
        let startup_src = &src[startup_start..];
        let client_idx = startup_src
            .find("Ok(Client {")
            .expect("Client construction not found");
        let capture_block = &startup_src[..client_idx];

        assert!(
            capture_block.contains("auth_outcome.pool_generation")
                && capture_block.contains("auth_outcome.pool_generation_is_dynamic"),
            "startup must carry the authenticated pool generation from auth; \
             post-auth global POOLS/DYNAMIC_POOLS re-reads race dynamic invalidation"
        );
        assert!(
            !capture_block.contains("crate::pool::get_pool_by_id(&cached_pool_id)")
                && !capture_block.contains("crate::pool::is_dynamic_pool(&cached_pool_id)"),
            "startup must not recapture dynamic pool routing with separate \
             lock-free global reads after authentication"
        );
    }

    /// Reproducer for the GSSENCRequest reject typo. PostgreSQL `postmaster.c`
    /// replies `'N'` (no-GSS,
    /// fall back) when GSSAPI is unavailable; libpq's `fe-connect.c`
    /// (`CONNECTION_GSS_STARTUP`) accepts `'N'` as the signal to retry
    /// startup on the SAME socket with SSLRequest or StartupMessage.
    /// pg_doorman wrote `'G'` (= "begin GSSAPI handshake"), then
    /// returned `Err(AuthError)`, which dropped the socket. libpq saw
    /// "server closed the connection unexpectedly" after sending its
    /// GSSAPI token to a half-closed FD. Symmetric to beeaea7. This
    /// test sends GSSENCRequest, asserts the reject byte is `'N'`
    /// (not `'G'`), and asserts the next startup-class message
    /// (here CancelRequest) is read on the same socket - i.e. the
    /// fixed arm loops back instead of returning Err.
    #[tokio::test]
    async fn gssenc_reject_writes_n_and_loops_back_to_next_message() {
        use super::{get_startup, ClientConnectionType};
        use crate::messages::constants::{CANCEL_REQUEST_CODE, REQUEST_GSSENCMODE_CODE};
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(1024);

        let client_task = tokio::spawn(async move {
            // 1. GSSENCRequest (length=8, code=REQUEST_GSSENCMODE_CODE).
            let mut req = Vec::with_capacity(8);
            req.extend_from_slice(&8i32.to_be_bytes());
            req.extend_from_slice(&REQUEST_GSSENCMODE_CODE.to_be_bytes());
            client.write_all(&req).await.expect("write GSSENCRequest");

            // 2. Read pg_doorman's reject byte - must be 'N' per
            // postmaster.c, NOT 'G' (which means "begin GSSAPI").
            let mut b = [0u8; 1];
            client.read_exact(&mut b).await.expect("read reject byte");
            assert_eq!(
                b[0], b'N',
                "GSSENC reject byte must be 'N' (no-GSS, fall back) per PG postmaster.c; \
                 'G' tells libpq to begin a GSSAPI handshake on a socket we're about to drop"
            );

            // 3. Send a follow-up CancelRequest on the same socket.
            // libpq (gssencmode=prefer + sslmode=disable, or a cancel
            // socket) sends a startup-class message right after our 'N'.
            let mut cancel = Vec::with_capacity(16);
            cancel.extend_from_slice(&16i32.to_be_bytes());
            cancel.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
            cancel.extend_from_slice(&7i32.to_be_bytes()); // pid
            cancel.extend_from_slice(&9i32.to_be_bytes()); // secret
            client
                .write_all(&cancel)
                .await
                .expect("write CancelRequest");
            let _ = client.flush().await;
        });

        let result = get_startup(&mut server).await;
        let _ = client_task.await;

        let (kind, _bytes) =
            result.expect("get_startup must loop back after GSS reject, not drop the socket");
        assert!(
            matches!(kind, ClientConnectionType::CancelQuery),
            "expected CancelQuery after GSS reject loop-back, got a different variant"
        );
    }
}
