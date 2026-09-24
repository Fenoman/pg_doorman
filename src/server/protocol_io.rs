//! PostgreSQL protocol I/O operations for server connections.
//!
//! This module handles communication with PostgreSQL servers, including:
//! - Sending messages to the server with timeout support
//! - Receiving and parsing server responses
//! - Handling large messages and COPY protocol
//! - Managing server state based on protocol messages

use std::mem;
use std::time::Duration;

use bytes::{Buf, BufMut, BytesMut};
use log::{error, info, warn};

/// RAII guard that flips `state_wait` back to idle on drop.
/// Used to hoist the per-iter `wait_reading()`/`wait_idle()` pair out
/// of the inner recv loop without losing the "idle once we return"
/// semantic observers depend on. Drop runs on every exit shape - early
/// break, return Ok / Err, panic, async cancel - so the observability
/// contract holds even on the failure paths the hot loop has.
///
/// Owns an `Arc<ServerStats>` (one refcount bump per recv invocation)
/// rather than a borrow so the rest of the recv loop keeps full
/// `&mut Server` access - borrowing `&server.stats` would block every
/// downstream mutation of `server`.
struct WaitIdleOnDrop(std::sync::Arc<crate::stats::ServerStats>);

impl Drop for WaitIdleOnDrop {
    #[inline]
    fn drop(&mut self) {
        self.0.wait_idle();
    }
}

/// Replace newlines and carriage returns to keep log lines single-line.
/// Characters of one PostgreSQL error field copied into a log line; the
/// client still receives the whole message.
const MAX_LOGGED_ERROR_CHARS: usize = 1024;

pub(crate) fn sanitize_for_log(s: &str) -> String {
    let (head, left_out) = match s.char_indices().nth(MAX_LOGGED_ERROR_CHARS) {
        Some((end, _)) => (&s[..end], s.len() - end),
        None => (s, 0),
    };
    let mut logged = if head.contains(['\n', '\r']) {
        head.replace('\n', "\\n").replace('\r', "\\r")
    } else {
        head.to_string()
    };
    if left_out > 0 {
        logged.push_str(&format!("... ({left_out} more bytes)"));
    }
    logged
}

use crate::errors::Error;
use crate::errors::Error::MaxMessageSize;
use crate::messages::socket::read_message_body_append;
use crate::messages::PgErrorMsg;
use crate::messages::MAX_MESSAGE_SIZE;
use crate::messages::{
    read_message_body_reuse, read_message_header, write_all_flush, write_all_flush_timeout,
    BytesMutReader,
};

use super::cleanup::{ResetCleanupCommand, SetCleanupCommand};
use super::parameters::ServerParameters;
use super::server_backend::Server;

// PostgreSQL CommandComplete message payloads for tracking session state changes.
//
// A checkin-time `RESET ALL` / `DEALLOCATE ALL` / `CLOSE ALL` is a heuristic
// upper bound: we arm the `needs_cleanup_*` flags when we see a statement that
// *might* have mutated the session, and we disarm them when we see a statement
// that has since restored it. Disarming matters because otherwise a client that
// performs its own reset batch (e.g. pgx on internal context deadline sends
// `SET SESSION AUTHORIZATION DEFAULT; RESET ALL; CLOSE ALL; UNLISTEN *;
// DISCARD PLANS; ...`) leaves pg_doorman thinking the connection is still dirty
// and triggers a second, redundant `RESET ALL` round-trip on checkin.
//
// PostgreSQL reports both `RESET ALL` and `RESET foo.bar` as the same `RESET`
// CommandComplete tag. Because the per-GUC form can leave other dirty GUCs such
// as `client.app_user` behind, the generic tag must not disarm SET cleanup.

/// `SET` statement CommandComplete tag — arms the `needs_cleanup_set` flag.
/// Returned for any `SET foo = ...`, including `SET SESSION AUTHORIZATION ...`.
const COMMAND_COMPLETE_BY_SET: &[u8; 4] = b"SET\0";
/// Both `RESET ALL` and narrower `RESET ...` statements produce this tag.
const COMMAND_COMPLETE_BY_RESET: &[u8; 6] = b"RESET\0";
/// `DECLARE CURSOR` CommandComplete tag — arms the `needs_cleanup_declare` flag.
const COMMAND_COMPLETE_BY_DECLARE: &[u8; 15] = b"DECLARE CURSOR\0";
/// SQL-level `PREPARE` CommandComplete tag - arms `needs_cleanup_prepare`.
const COMMAND_COMPLETE_BY_PREPARE: &[u8; 8] = b"PREPARE\0";
/// `CLOSE ALL` CommandComplete tag - disarms `needs_cleanup_declare`.
/// Note the server emits `CLOSE CURSOR ALL`, not `CLOSE ALL`.
const COMMAND_COMPLETE_BY_CLOSE_CURSOR_ALL: &[u8; 17] = b"CLOSE CURSOR ALL\0";
/// `DEALLOCATE ALL` CommandComplete tag — clears prepared statement cache
/// and disarms `needs_cleanup_prepare`.
const COMMAND_COMPLETE_BY_DEALLOCATE_ALL: &[u8; 15] = b"DEALLOCATE ALL\0";
const COMMAND_COMPLETE_BY_DEALLOCATE: &[u8; 11] = b"DEALLOCATE\0";
/// `DISCARD ALL` CommandComplete tag — equivalent to `RESET ALL; DEALLOCATE ALL;
/// CLOSE ALL; UNLISTEN *; ...`, so disarms every `needs_cleanup_*` flag.
const COMMAND_COMPLETE_BY_DISCARD_ALL: &[u8; 12] = b"DISCARD ALL\0";

/// Flushes messages, allowing at most `duration` without the backend
/// accepting more bytes; a stall marks the server bad.
/// A write this large gets the release deferred to it in a write of its
/// own instead of a copy with the release in front.
const DEFERRED_RELEASE_COPY_LIMIT: usize = 64 * 1024;

/// The release the last check-in deferred to this write, if any: copied in
/// front of a small write (`Joined`), or to go alone just before a large one.
enum DeferredRelease<'a> {
    None(&'a [u8]),
    Joined(Vec<u8>),
    Ahead(bytes::Bytes, &'a [u8]),
}

impl<'a> DeferredRelease<'a> {
    fn take(server: &mut Server, messages: &'a [u8]) -> Result<Self, Error> {
        Ok(match server.take_deferred_release()? {
            None => Self::None(messages),
            Some(release) if messages.len() <= DEFERRED_RELEASE_COPY_LIMIT => {
                let mut joined = Vec::with_capacity(release.len() + messages.len());
                joined.extend_from_slice(&release);
                joined.extend_from_slice(messages);
                Self::Joined(joined)
            }
            Some(release) => Self::Ahead(release, messages),
        })
    }

    fn parts(&self) -> (Option<&[u8]>, &[u8]) {
        match self {
            Self::None(messages) => (None, messages),
            Self::Joined(joined) => (None, joined),
            Self::Ahead(release, messages) => (Some(release), messages),
        }
    }
}

pub(crate) async fn send_and_flush_timeout(
    server: &mut Server,
    messages: &[u8],
    duration: Duration,
) -> Result<(), Error> {
    let deferred = DeferredRelease::take(server, messages)?;
    let (release_ahead, messages) = deferred.parts();
    if let Some(release) = release_ahead {
        server.stats.data_sent(release.len());
    }
    server.stats.data_sent(messages.len());
    server.stats.wait_writing();

    // Bytes already buffered go first; the messages then bypass the buffer,
    // since a flush of the buffered writer hides how far it got.
    let result = async {
        use tokio::io::AsyncWriteExt;

        match crate::utils::timeout::timeout_unless_ready(duration, server.stream.flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Err(Error::SocketError(format!(
                    "Error flushing socket: {err:?}"
                )))
            }
            Err(_) => return Err(Error::ProxyTimeout),
        }
        if let Some(release) = release_ahead {
            write_all_flush_timeout(server.stream.get_mut(), release, duration).await?;
        }
        write_all_flush_timeout(server.stream.get_mut(), messages, duration).await
    }
    .await;
    server.stats.wait_idle();
    match result {
        Ok(()) => {
            server.touch_activity();
            Ok(())
        }
        Err(Error::ProxyTimeout) => {
            server.mark_bad("flush timeout");
            error!(
                "[{}@{}] flush timeout pid={}: no progress for {duration:?}",
                server.address.username,
                server.address.pool_name,
                server.get_process_id(),
            );
            Err(Error::FlushTimeout)
        }
        Err(err) => {
            error!(
                "[{}@{}] server connection terminated pid={}: {err}",
                server.address.username,
                server.address.pool_name,
                server.get_process_id(),
            );
            server.mark_bad("failed to flush data to server");
            Err(err)
        }
    }
}

/// Flushes messages and records write stats/activity.
pub(crate) async fn send_and_flush(server: &mut Server, messages: &[u8]) -> Result<(), Error> {
    let deferred = DeferredRelease::take(server, messages)?;
    let (release_ahead, messages) = deferred.parts();
    if let Some(release) = release_ahead {
        server.stats.data_sent(release.len());
    }
    server.stats.data_sent(messages.len());
    server.stats.wait_writing();

    let written = async {
        if let Some(release) = release_ahead {
            write_all_flush(&mut *server.stream, release).await?;
        }
        write_all_flush(&mut *server.stream, messages).await
    };
    match written.await {
        Ok(_) => {
            // Successfully sent to server
            server.stats.wait_idle();
            server.touch_activity();
            Ok(())
        }
        Err(err) => {
            server.stats.wait_idle();
            error!(
                "[{}@{}] server connection terminated pid={}: {err}",
                server.address.username,
                server.address.pool_name,
                server.get_process_id(),
            );
            server.mark_bad("failed to flush data to server");
            Err(err)
        }
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Handles large DataRow ('D') messages that exceed max_message_size.
/// Streams the message directly to the client without buffering.
async fn handle_large_data_row<C>(
    server: &mut Server,
    client_stream: &mut C,
    code_u8: u8,
    message_len: i32,
) -> Result<BytesMut, Error>
where
    C: tokio::io::AsyncWrite + std::marker::Unpin,
{
    let copy_timeout = crate::config::proxy_copy_data_timeout();
    let client_error = stream_large_frame(
        server,
        client_stream,
        code_u8,
        message_len,
        copy_timeout,
        "data_row",
    )
    .await?;
    server.data_available = true;
    streamed_frame_result(client_error)
}

/// The response to a streamed frame: nothing more to hand back, or the
/// error of a client that went away mid-frame.
fn streamed_frame_result(client_error: Option<Error>) -> Result<BytesMut, Error> {
    match client_error {
        None => Ok(BytesMut::new()),
        Some(err) => Err(Error::ClientGoneMidStream(err.to_string())),
    }
}

/// Streams a frame larger than `max_message_size` from the backend to the
/// client: the response gathered so far with the frame header, then the
/// body in chunks. A client that stops taking bytes (an error, or no
/// progress for `timeout`) gets no more, but the rest of the frame is still
/// read, so the backend stays in step with the protocol and its query can
/// be drained or canceled while the pool slot is still held. Returns the
/// client's error in that case; fails only when the backend does.
async fn stream_large_frame<C>(
    server: &mut Server,
    client_stream: &mut C,
    code_u8: u8,
    message_len: i32,
    timeout: Duration,
    kind: &'static str,
) -> Result<Option<Error>, Error>
where
    C: tokio::io::AsyncWrite + std::marker::Unpin,
{
    use tokio::io::AsyncReadExt;

    server.buffer.put_u8(code_u8);
    server.buffer.put_i32(message_len);
    let prev_bad = server.bad;
    // A future dropped mid-frame leaves the stream inside it.
    server.bad = true;
    const HEADER_BYTES: usize = 1 + mem::size_of::<i32>();
    let mut written = 0;
    let mut client_error = crate::messages::socket::write_all_flush_timeout_counted(
        client_stream,
        &server.buffer,
        timeout,
        &mut written,
    )
    .await
    .err();
    // Bytes of this frame the client took; its header ends the write.
    let mut delivered = written.saturating_sub(server.buffer.len() - HEADER_BYTES);
    // Once the client has gone, the rest of the frame gets the time an
    // abandoned query gets. A backend sending it slower is closed; being in
    // the middle of a write, PostgreSQL notices that at once.
    let abandoned = server.abandoned_query_timeouts;
    let drain_budget = abandoned.finish + abandoned.after_cancel;
    let mut drain_deadline = client_error
        .is_some()
        .then(|| tokio::time::Instant::now() + drain_budget);

    const MAX_CHUNK: usize = 65536;
    let mut remaining = message_len as usize - mem::size_of::<i32>();
    let mut chunk = vec![0_u8; remaining.min(MAX_CHUNK)];
    let mut backend_error = None;
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        let wait = drain_deadline.map_or(timeout, |deadline| {
            timeout.min(deadline.saturating_duration_since(tokio::time::Instant::now()))
        });
        let read = match crate::utils::timeout::timeout_unless_ready(
            wait,
            server.stream.read(&mut chunk[..want]),
        )
        .await
        {
            Ok(Ok(0)) => Err(Error::SocketError(
                "Error reading from socket: connection closed".to_string(),
            )),
            Ok(Ok(read)) => Ok(read),
            Ok(Err(err)) => Err(Error::SocketError(format!(
                "Error reading from socket: {err:?}"
            ))),
            Err(_) => Err(Error::ProxyTimeout),
        };
        let read = match read {
            Ok(read) => read,
            Err(err) => {
                backend_error = Some(err);
                break;
            }
        };
        remaining -= read;
        if client_error.is_none() {
            match crate::messages::socket::write_all_flush_timeout_counted(
                client_stream,
                &chunk[..read],
                timeout,
                &mut delivered,
            )
            .await
            {
                Ok(()) => {}
                Err(err) => {
                    client_error = Some(err);
                    drain_deadline = Some(tokio::time::Instant::now() + drain_budget);
                }
            }
        }
    }
    record_streaming(
        server,
        kind,
        backend_error.is_none() && client_error.is_none(),
        delivered as u64,
    );
    if let Some(err) = backend_error {
        server.mark_bad(err.to_string().as_str());
        return Err(err);
    }
    server.bad = prev_bad;
    server
        .stats
        .data_received(server.buffer.len() + message_len as usize);
    server.touch_activity();
    server.stats.wait_idle();
    server.buffer.clear();
    Ok(client_error)
}

/// Handles large FunctionCallResponse ('V') messages that exceed max_message_size.
/// Streams the message directly to the client without buffering.
async fn handle_large_function_call_response<C>(
    server: &mut Server,
    client_stream: &mut C,
    code_u8: u8,
    message_len: i32,
) -> Result<BytesMut, Error>
where
    C: tokio::io::AsyncWrite + std::marker::Unpin,
{
    let copy_timeout = crate::config::proxy_copy_data_timeout();
    let client_error = stream_large_frame(
        server,
        client_stream,
        code_u8,
        message_len,
        copy_timeout,
        "function_call_response",
    )
    .await?;
    server.data_available = true;
    streamed_frame_result(client_error)
}

/// Handles large CopyData ('d') messages that exceed max_message_size.
/// Streams the message directly to the client without buffering.
async fn handle_large_copy_data<C>(
    server: &mut Server,
    client_stream: &mut C,
    code_u8: u8,
    message_len: i32,
) -> Result<BytesMut, Error>
where
    C: tokio::io::AsyncWrite + std::marker::Unpin,
{
    let copy_timeout = crate::config::proxy_copy_data_timeout();
    handle_large_copy_data_inner(server, client_stream, code_u8, message_len, copy_timeout).await
}

/// Inner body of [`handle_large_copy_data`] with the COPY-data stream
/// deadline injected, so unit tests can drive it with a short timeout
/// instead of the configured production value.
async fn handle_large_copy_data_inner<C>(
    server: &mut Server,
    client_stream: &mut C,
    code_u8: u8,
    message_len: i32,
    copy_timeout: Duration,
) -> Result<BytesMut, Error>
where
    C: tokio::io::AsyncWrite + std::marker::Unpin,
{
    // Bounded by `proxy_copy_data_timeout` like the other frames: a backend
    // that stalls mid-frame on a live but silent socket fails the stream and
    // is evicted instead of pinning this task.
    let client_error = stream_large_frame(
        server,
        client_stream,
        code_u8,
        message_len,
        copy_timeout,
        "copy_data",
    )
    .await?;
    streamed_frame_result(client_error)
}

/// Helper that bumps both streaming counters from the streaming handlers.
/// `kind` is "data_row", "copy_data", or "function_call_response"; the
/// boolean carries the proxy outcome and is mapped to the "ok"/"error" label.
fn record_streaming(server: &Server, kind: &'static str, ok: bool, total_bytes: u64) {
    let user = server.address.username.as_str();
    let database = server.address.database.as_str();
    let result = if ok { "ok" } else { "error" };
    crate::web::metrics::observe_streaming_event(user, database, kind, result);
    crate::web::metrics::observe_streaming_bytes(user, database, kind, total_bytes);
}

/// Handles ReadyForQuery ('Z') message - indicates server is ready for a new query.
/// Updates transaction state based on the transaction status indicator.
fn handle_ready_for_query(server: &mut Server, message: &mut BytesMut) -> Result<(), Error> {
    // a backend Z frame with len=4 (claiming empty body)
    // passes `read_message_body_reuse` (len >= 4 gate) but leaves
    // `message` empty here - `get_u8` then panicked. PG itself never
    // emits this; observed from corrupted backend / MITM / buggy
    // proxies. Mark the backend bad and return Err so the connection
    // is evicted cleanly.
    if !message.has_remaining() {
        server.mark_bad("malformed ReadyForQuery: no transaction-state byte");
        return Err(Error::ProtocolSyncError(
            "ReadyForQuery missing transaction-state byte".to_string(),
        ));
    }
    let transaction_state = message.get_u8() as char;

    match transaction_state {
        // 'T' - In transaction block
        'T' => {
            server.in_transaction = true;
            server.command_complete_in_transaction = true;
        }

        // 'I' - Idle (not in transaction)
        'I' => {
            server.in_transaction = false;
            server.command_complete_in_transaction = false;
        }

        // 'E' - In failed transaction block (requires ROLLBACK)
        'E' => {
            server.in_transaction = true;
            server.command_complete_in_transaction = true;
            if let Ok(msg) = PgErrorMsg::parse(message) {
                let mut details =
                    format!(
                    "[{}@{}] transaction rolled back pid={}: severity={}, code={}, message=\"{}\"",
                    server.address.username, server.address.pool_name, server.get_process_id(),
                    msg.severity, msg.code, sanitize_for_log(&msg.message),
                );
                if let Some(ref hint) = msg.hint {
                    details.push_str(&format!(", hint=\"{}\"", sanitize_for_log(hint)));
                }
                error!("{details}");
            } else {
                error!(
                    "[{}@{}] transaction error pid={}: could not parse error details",
                    server.address.username,
                    server.address.pool_name,
                    server.get_process_id(),
                );
            }
        }

        // Unknown transaction state - protocol error
        _ => {
            let err = Error::ProtocolSyncError(format!(
                "Protocol synchronization error with server {} (database: {}, user: {}). Received unknown transaction state character: '{}' (ASCII: {}). This may indicate an incompatible PostgreSQL server version or a corrupted message.",
                server.address.host,
                server.address.database,
                server.address.username,
                transaction_state,
                transaction_state as u8
            ));
            error!("{err}");
            server.mark_bad(
                format!("Protocol sync error: unknown transaction state '{transaction_state}'")
                    .as_str(),
            );
            return Err(err);
        }
    };

    if transaction_state == 'I' && !server.response_cycle_had_error {
        if server.pending_cleanup_disarms.set {
            server.cleanup_state.needs_cleanup_set = false;
        }
        if server.pending_cleanup_disarms.startup_parameter_mirror {
            server
                .server_parameters
                .remove_startup_only_params_after_session_reset();
        }
        if server.pending_cleanup_disarms.role {
            server.cleanup_state.needs_cleanup_role = false;
        }
        if server.pending_cleanup_disarms.session_authorization {
            server.cleanup_state.needs_cleanup_session_authorization = false;
        }
    }
    server.pending_cleanup_disarms.clear();
    server.response_cycle_had_error = false;

    // No more data available from the server after ReadyForQuery
    server.data_available = false;
    server.clear_queued_statements_reset();
    server.clear_set_cleanup_commands();
    server.clear_reset_cleanup_commands();
    Ok(())
}

fn command_complete_enters_transaction(message: &[u8]) -> bool {
    matches!(message, b"BEGIN\0" | b"START TRANSACTION\0")
}

fn track_command_complete_transaction_state(server: &mut Server, message: &[u8]) {
    if command_complete_enters_transaction(message) {
        server.command_complete_in_transaction = true;
    }
}

/// SQLSTATEs showing that the backend's prepared statements no longer match
/// what the pooler believes: a cached plan whose result shape changed under
/// DDL (0A000), a statement missing (26000) or already present (42P05). An
/// error without a SQLSTATE proves nothing about them and counts as stale.
fn error_invalidates_prepared_statements(sqlstate: &str) -> bool {
    sqlstate.len() != 5 || matches!(sqlstate, "0A000" | "26000" | "42P05")
}

/// Handles ErrorResponse ('E') message from the server.
/// Logs the error and updates server state accordingly.
fn handle_error_response(server: &mut Server, message: &mut BytesMut) {
    server.response_cycle_had_error = true;
    let mut recoverable = false;
    // An unreadable error proves nothing about the statements: keep the reset.
    let mut invalidates_prepared_statements = true;
    if let Ok(msg) = PgErrorMsg::parse(message) {
        recoverable = msg.severity == "ERROR"
            || (msg.severity.is_empty() && msg.severity_localized == "ERROR");
        // A missing statement the pooler named on purpose, to get
        // PostgreSQL's error in its place, says nothing about the others.
        invalidates_prepared_statements = error_invalidates_prepared_statements(&msg.code)
            && !msg
                .message
                .contains(crate::client::util::UNREGISTERED_STATEMENT_PREFIX);
        let logged_message = sanitize_for_log(&msg.message);
        let mut details = format!(
            "[{}@{}] server error pid={}: severity={}, code={}, message=\"{}\", in_transaction={}, in_copy={}",
            server.address.username, server.address.pool_name, server.get_process_id(),
            msg.severity, msg.code, logged_message,
            server.in_transaction, server.in_copy_mode,
        );
        if let Some(ref hint) = msg.hint {
            details.push_str(&format!(", hint=\"{}\"", sanitize_for_log(hint)));
        }
        if let Some(ref detail) = msg.detail {
            details.push_str(&format!(", detail=\"{}\"", sanitize_for_log(detail)));
        }
        error!("{details}");
        server.address.stats.error_with_sqlstate(&msg.code);
        // do NOT bump `server.stats.error()` on
        // every PG ErrorResponse - that includes routine application
        // SQL errors (`23xxx` unique violation, `40xxx` serialization
        // failure, `42xxx` syntax) which would inflate SHOW SERVERS
        // error_count to meaninglessness. Per-SQLSTATE breakdown via
        // `pg_doorman_pools_errors_total{sqlstate}` is the correct
        // surface for SQL-level errors.
        // Let `small_simple_query` return SQL-level failures as `Err`. Its
        // callers only log the text or wrap it in their own errors and never
        // send it to the client, so the text cut for the log is enough.
        server.last_sql_error = Some((msg.code.clone(), logged_message));
    } else {
        error!(
            "[{}@{}] server error pid={}: could not parse error details",
            server.address.username,
            server.address.pool_name,
            server.get_process_id(),
        );
        server.address.stats.error();
        // unparseable ErrorResponse IS a pooler-level
        // problem (protocol desync), so this branch correctly bumps
        // the per-server error_count.
        server.stats.error();
        // SQLSTATE XX000 = `internal_error`: closest standard match for
        // "PG sent an ErrorResponse we couldn't parse". 00000 would
        // collide with `successful_completion` and pollute SQLSTATE
        // dashboards.
        server.last_sql_error = Some((
            "XX000".to_string(),
            "<unparseable ErrorResponse>".to_string(),
        ));
    }

    // Exit COPY mode on error
    if server.in_copy_mode {
        server.in_copy_mode = false;
    }

    // DEALLOCATE ALL at check-in drops every statement on the backend,
    // including the DOORMAN_N other clients share, and with cleanup disabled
    // closes the backend instead. An ordinary statement error (unique
    // violation, serialization failure) leaves the statements valid; a
    // rejected Parse is rolled back precisely below.
    if server.prepared_statement_cache.is_some() && invalidates_prepared_statements {
        server.cleanup_state.needs_cleanup_prepare = true;
    }

    // A Parse error means PostgreSQL did not install any pending prepared
    // statement names. Drop the optimistic LRU entries so the next Bind
    // re-Parses instead of hitting a stale DOORMAN_N.
    if !server.registering_prepared_statement.is_empty() {
        let pending: Vec<String> = server
            .registering_prepared_statement
            .drain(..)
            .map(|pending| pending.name)
            .collect();
        server
            .rejected_prepared_statement_names
            .extend(pending.iter().cloned());
        for name in &pending {
            server.remove_prepared_statement_from_cache(name);
        }
        // Nothing pending remains to reconcile at check-in.
        server.has_pending_cache_entries = false;
    }

    // Handle async mode errors
    if server.is_async() {
        server.data_available = false;
        // Keep cleanup attribution armed while the same client owns the
        // backend until Sync. An ordinary SQL error is recoverable and must
        // not discard the session's warm temp state at that later checkin.
        // Prepared statements need no blanket reset: the decision above and
        // the rolled-back registrations already cover them.
        let needs_cleanup_prepare = server.cleanup_state.needs_cleanup_prepare;
        server.cleanup_state.set_true();
        server.cleanup_state.needs_cleanup_prepare = needs_cleanup_prepare;
        if !recoverable && !server.session_mode {
            server.mark_bad("PostgreSQL error in asynchronous operation mode");
        }
    }
}

/// Effect a single CommandComplete tag has on the server's cleanup tracking.
///
/// Extracted from [`handle_command_complete`] so the tag-matching logic can be
/// unit-tested without constructing a full `Server`. See the tests at the bottom
/// of this file for the exhaustive tag coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandCompleteEffect {
    /// Tag does not influence cleanup tracking (e.g. SELECT, INSERT).
    None,
    /// `SET ...` — session GUC potentially mutated; arm set-cleanup.
    ArmSet,
    /// `SET ROLE ...` - current role potentially mutated.
    ArmRole,
    /// `SET SESSION AUTHORIZATION ...` - session identity potentially mutated.
    ArmSessionAuthorization,
    /// `SET ROLE DEFAULT/NONE` or `RESET ROLE` restored current role.
    DisarmRole,
    /// `SET/RESET SESSION AUTHORIZATION DEFAULT` restored session identity.
    DisarmSessionAuthorization,
    /// `DECLARE CURSOR` - a server-side cursor may now be open; arm declare-cleanup.
    ArmDeclare,
    /// SQL-level `PREPARE` - a server-side prepared statement may now exist.
    ArmPrepare,
    /// SQL-level `DEALLOCATE <name>` - one prepared statement is gone.
    DeallocateOne,
    /// Proven `RESET ALL` - every GUC tracked by SET cleanup has been restored.
    DisarmSet,
    /// `CLOSE CURSOR ALL` — no server-side cursors remain; disarm declare-cleanup.
    DisarmDeclare,
    /// `DEALLOCATE ALL` — every prepared statement is gone server-side; disarm
    /// prepare-cleanup and drop the LRU so the next checkout starts from scratch.
    DisarmPrepare,
    /// `DISCARD ALL` — equivalent to `RESET ALL; DEALLOCATE ALL; CLOSE ALL;
    /// UNLISTEN *; ...` executed atomically; disarm every `needs_cleanup_*` flag
    /// and drop the LRU.
    DisarmAll,
}

/// Pure classifier for CommandComplete tags relevant to session cleanup tracking.
///
/// The tags are compared byte-for-byte; `PartialEq for [u8]` already short-circuits
/// on length, so non-matching messages (the common case on the hot path) cost a
/// single length comparison per arm.
#[cfg(test)]
fn classify_command_complete(tag: &[u8]) -> CommandCompleteEffect {
    classify_command_complete_with_attribution(tag, None, None)
}

#[cfg(test)]
fn classify_command_complete_with_reset_attribution(
    tag: &[u8],
    reset_command: Option<ResetCleanupCommand>,
) -> CommandCompleteEffect {
    classify_command_complete_with_attribution(tag, None, reset_command)
}

fn classify_command_complete_with_attribution(
    tag: &[u8],
    set_command: Option<SetCleanupCommand>,
    reset_command: Option<ResetCleanupCommand>,
) -> CommandCompleteEffect {
    if tag == COMMAND_COMPLETE_BY_SET {
        match set_command {
            Some(SetCleanupCommand::GenericSet) | None => CommandCompleteEffect::ArmSet,
            Some(SetCleanupCommand::SetRole) => CommandCompleteEffect::ArmRole,
            Some(SetCleanupCommand::SetRoleDefault) => CommandCompleteEffect::DisarmRole,
            Some(SetCleanupCommand::SetSessionAuthorization) => {
                CommandCompleteEffect::ArmSessionAuthorization
            }
            Some(SetCleanupCommand::SetSessionAuthorizationDefault) => {
                CommandCompleteEffect::DisarmSessionAuthorization
            }
        }
    } else if tag == COMMAND_COMPLETE_BY_RESET {
        match reset_command {
            Some(ResetCleanupCommand::ResetAll) => CommandCompleteEffect::DisarmSet,
            Some(ResetCleanupCommand::ResetRole) => CommandCompleteEffect::DisarmRole,
            Some(ResetCleanupCommand::ResetSessionAuthorization) => {
                CommandCompleteEffect::DisarmSessionAuthorization
            }
            Some(ResetCleanupCommand::PerGucReset) | None => CommandCompleteEffect::None,
        }
    } else if tag == COMMAND_COMPLETE_BY_DECLARE {
        CommandCompleteEffect::ArmDeclare
    } else if tag == COMMAND_COMPLETE_BY_PREPARE {
        CommandCompleteEffect::ArmPrepare
    } else if tag == COMMAND_COMPLETE_BY_CLOSE_CURSOR_ALL {
        CommandCompleteEffect::DisarmDeclare
    } else if tag == COMMAND_COMPLETE_BY_DEALLOCATE_ALL {
        CommandCompleteEffect::DisarmPrepare
    } else if tag == COMMAND_COMPLETE_BY_DEALLOCATE {
        CommandCompleteEffect::DeallocateOne
    } else if tag == COMMAND_COMPLETE_BY_DISCARD_ALL {
        CommandCompleteEffect::DisarmAll
    } else {
        CommandCompleteEffect::None
    }
}

/// Drop the pg_doorman-side prepared statement LRU after the server confirms it
/// just executed an equivalent of `DEALLOCATE ALL` or `DISCARD ALL`.
///
/// Parses confirmed before the reset have already left
/// `registering_prepared_statement`; what is still pending comes later in
/// the same pipeline and creates its statement after the reset, so those
/// registrations and their cache entries stay.
fn drop_prepared_statement_cache_on_reset(server: &mut Server, reason: &'static str) {
    server.release_statements_prepared = false;
    server.clear_queued_statements_reset();
    let Some(cache_size) = server
        .prepared_statement_cache
        .as_ref()
        .map(|cache| cache.len())
    else {
        server.registering_prepared_statement.clear();
        return;
    };
    warn!(
        "[{}@{}] clearing prepared statement cache pid={}: {reason} ({cache_size} entries)",
        server.address.username,
        server.address.pool_name,
        server.get_process_id(),
    );
    if let Some(cache) = server.prepared_statement_cache.as_mut() {
        cache.clear();
        for pending in &server.registering_prepared_statement {
            cache.put(pending.name.clone(), ());
        }
    }
}

/// A disarm may be recorded once the statement that produced it can no longer
/// be rolled back.
///
/// Client traffic is gated on the transaction mirrors: a `RESET` inside an open
/// transaction is undone if that transaction aborts, so its disarm must wait for
/// the idle `ReadyForQuery` that commits it.
///
/// The pooler's own housekeeping round trip is different. When a backend is
/// checked in while still in a transaction (the only way a client can abandon
/// one in transaction pooling), `collect_checkin_cleanup_sqls` prepends
/// `ROLLBACK`, so every following `RESET` in that batch runs outside a
/// transaction and its effect is already final. The mirrors, however, are only
/// refreshed by `ReadyForQuery`, which arrives at the very END of the batch —
/// gating on them here would drop every disarm the batch produced and make
/// `finalize_checkin` conclude that the release query left the session dirty,
/// destroying a healthy backend on every abnormal client exit.
fn cleanup_disarm_is_transactionally_safe(server: &Server) -> bool {
    server.internal_round_trip_in_flight()
        || (!server.in_transaction() && !server.command_complete_in_transaction)
}

fn defer_set_cleanup_disarm_if_transactionally_safe(server: &mut Server) {
    if !cleanup_disarm_is_transactionally_safe(server) {
        return;
    }
    server.pending_cleanup_disarms.set = true;
    server.pending_cleanup_disarms.startup_parameter_mirror = true;
}

fn defer_role_cleanup_disarm_if_transactionally_safe(server: &mut Server) {
    if !cleanup_disarm_is_transactionally_safe(server) {
        return;
    }
    server.pending_cleanup_disarms.role = true;
}

fn defer_session_authorization_cleanup_disarm_if_transactionally_safe(server: &mut Server) {
    if !cleanup_disarm_is_transactionally_safe(server) {
        return;
    }
    server.pending_cleanup_disarms.session_authorization = true;
    server.pending_cleanup_disarms.role = true;
}

/// Handles CommandComplete ('C') message - indicates successful completion of a command.
/// Tracks commands that may require cleanup (SET, DECLARE, ...) and disarms the
/// cleanup flags when the session has since been restored by a DISCARD /
/// DEALLOCATE / CLOSE ALL statement in the same or a later batch. A generic
/// `RESET` tag is intentionally not enough because PostgreSQL uses it for both
/// `RESET ALL` and per-GUC resets.
fn handle_command_complete(server: &mut Server, message: &BytesMut) {
    // Exit COPY mode if we were in it
    if server.in_copy_mode {
        server.in_copy_mode = false;
    }
    track_command_complete_transaction_state(server, &message[..]);

    let set_command = if &message[..] == COMMAND_COMPLETE_BY_SET {
        server.pop_set_cleanup_command()
    } else {
        None
    };
    let reset_command = if &message[..] == COMMAND_COMPLETE_BY_RESET {
        server.pop_reset_cleanup_command()
    } else {
        None
    };

    match classify_command_complete_with_attribution(&message[..], set_command, reset_command) {
        CommandCompleteEffect::None => {}
        CommandCompleteEffect::ArmSet => {
            server.cleanup_state.needs_cleanup_set = true;
            server.pending_cleanup_disarms.set = false;
        }
        CommandCompleteEffect::ArmRole => {
            server.cleanup_state.needs_cleanup_role = true;
            server.pending_cleanup_disarms.role = false;
        }
        CommandCompleteEffect::ArmSessionAuthorization => {
            server.cleanup_state.needs_cleanup_session_authorization = true;
            server.cleanup_state.needs_cleanup_role = true;
            server.pending_cleanup_disarms.session_authorization = false;
            server.pending_cleanup_disarms.role = false;
        }
        CommandCompleteEffect::DisarmRole => {
            defer_role_cleanup_disarm_if_transactionally_safe(server);
        }
        CommandCompleteEffect::DisarmSessionAuthorization => {
            defer_session_authorization_cleanup_disarm_if_transactionally_safe(server);
        }
        CommandCompleteEffect::ArmDeclare => {
            server.cleanup_state.needs_cleanup_declare = true;
        }
        CommandCompleteEffect::ArmPrepare => {
            server.cleanup_state.needs_cleanup_prepare = true;
            server.cleanup_state.sql_prepared_statements = server
                .cleanup_state
                .sql_prepared_statements
                .saturating_add(1);
        }
        CommandCompleteEffect::DeallocateOne => {
            if !server.cleanup_state.client_named_protocol_statements {
                server.cleanup_state.sql_prepared_statements = server
                    .cleanup_state
                    .sql_prepared_statements
                    .saturating_sub(1);
            }
        }
        CommandCompleteEffect::DisarmSet => {
            defer_set_cleanup_disarm_if_transactionally_safe(server);
        }
        CommandCompleteEffect::DisarmDeclare => {
            server.cleanup_state.needs_cleanup_declare = false;
        }
        // A named Parse later in the same pipeline was forwarded before this
        // reset was answered, and its statement outlives the reset: the mark
        // of protocol statements under client names survives it, and the
        // pooler's own DEALLOCATE ALL, which clears the mark, stays armed.
        CommandCompleteEffect::DisarmPrepare => {
            server.cleanup_state.needs_cleanup_prepare =
                server.cleanup_state.client_named_protocol_statements;
            server.cleanup_state.sql_prepared_statements = 0;
            drop_prepared_statement_cache_on_reset(server, "DEALLOCATE ALL");
        }
        CommandCompleteEffect::DisarmAll => {
            let named_protocol_statements = server.cleanup_state.client_named_protocol_statements;
            server.cleanup_state.reset();
            server.cleanup_state.client_named_protocol_statements = named_protocol_statements;
            server.cleanup_state.needs_cleanup_prepare = named_protocol_statements;
            server
                .server_parameters
                .remove_startup_only_params_after_session_reset();
            drop_prepared_statement_cache_on_reset(server, "DISCARD ALL");
        }
    }
}

/// Handles ParameterStatus ('S') message - server runtime parameter change notification.
/// Updates both server and client parameter tracking.
fn handle_parameter_status(
    server: &mut Server,
    message: &mut BytesMut,
    client_server_parameters: &mut Option<&mut ServerParameters>,
) -> Result<(), Error> {
    // hot-path ParameterStatus is reached on every backend `SET`,
    // GUC change, autovacuum notification. A truncated `S` frame from the
    // backend (network corruption, MITM, buggy proxy) used to panic here
    // and (via the panic hook) terminate the whole pooler.
    // Mark the backend bad on parse failure and abort this iteration; the
    // caller's recv loop will drop the broken connection cleanly.
    let key = match message.read_string() {
        Ok(k) => k,
        Err(err) => {
            log::warn!(
                "[{}@{}] malformed ParameterStatus key from server pid={}: {err}",
                server.address.username,
                server.address.pool_name,
                server.get_process_id()
            );
            server.mark_bad("malformed ParameterStatus key");
            return Err(Error::ProtocolSyncError(format!(
                "malformed ParameterStatus key: {err}"
            )));
        }
    };
    let value = match message.read_string() {
        Ok(v) => v,
        Err(err) => {
            log::warn!(
                "[{}@{}] malformed ParameterStatus value from server pid={}: {err}",
                server.address.username,
                server.address.pool_name,
                server.get_process_id()
            );
            server.mark_bad("malformed ParameterStatus value");
            return Err(Error::ProtocolSyncError(format!(
                "malformed ParameterStatus value: {err}"
            )));
        }
    };

    // Update client parameters if tracking is enabled
    if let Some(client_server_parameters) = client_server_parameters.as_mut() {
        client_server_parameters.set_param(&key, &value, false);
        if server.log_client_parameter_status_changes {
            info!(
                "[{}@{}] parameter changed pid={}: {key}={value}",
                server.address.username,
                server.address.pool_name,
                server.get_process_id()
            )
        }
    }

    // Always update server parameters
    server.server_parameters.set_param(key, value, false);
    Ok(())
}

/// Reads the reply to the release query the last check-in sent without
/// waiting for it: ParseComplete, BindComplete, rows and CommandComplete for
/// BEGIN, each statement and COMMIT, then EmptyQueryResponse and
/// ReadyForQuery. PostgreSQL sends it before it reads anything sent later,
/// so it comes ahead of every other reply.
///
/// An ErrorResponse means the release failed and PostgreSQL skipped all
/// that followed it up to a Sync, unexecuted. The backend is marked bad and
/// the exchange fails with `ReleaseQueryFailed`; its own reply is not read.
pub(crate) async fn settle_release_reply(server: &mut Server) -> Result<(), Error> {
    let result = read_release_reply(server).await;
    record_release_reply_metric(server, &result);
    result
}

/// Observes the check-in whose release reply was just read, or failed to be,
/// under the path it took and with the time its send took.
pub(crate) fn record_release_reply_metric(server: &mut Server, result: &Result<(), Error>) {
    if let Some((path, seconds)) = server.release_reply_metric.take() {
        server.record_checkin_cleanup_metric(
            path,
            Server::checkin_cleanup_metric_result(result),
            seconds,
        );
    }
}

async fn read_release_reply(server: &mut Server) -> Result<(), Error> {
    while server.release_reply_pending {
        let read = async {
            let (code, len) = read_message_header(&mut *server.stream).await?;
            if len >= MAX_MESSAGE_SIZE {
                return Err(MaxMessageSize);
            }
            read_message_body_reuse(&mut *server.stream, &mut server.read_buf, code, len).await
        };
        let mut message = match read.await {
            Ok(message) => message,
            Err(err) => {
                server.release_reply_pending = false;
                server.mark_bad(&format!("failed to read the release_query reply: {err}"));
                return Err(err);
            }
        };
        server.stats.data_received(message.len());
        let code = message.get_u8();
        let _len = message.get_i32();
        match code {
            b'1' | b'2' | b'D' | b'I' | b'N' | b'A' => {}
            b'C' => {
                // Sent ahead of a client's messages, the release ends with
                // its last CommandComplete; theirs follow.
                if server.release_reply_commands > 0 {
                    server.release_reply_commands -= 1;
                    if server.release_reply_commands == 0 {
                        server.release_reply_pending = false;
                    }
                }
            }
            b'S' => handle_parameter_status(server, &mut message, &mut None)?,
            b'Z' if server.release_reply_commands > 0 => {
                server.release_reply_pending = false;
                server.mark_bad("ReadyForQuery inside the release_query reply");
                return Err(Error::ProtocolSyncError(
                    "ReadyForQuery inside the release_query reply".to_string(),
                ));
            }
            b'Z' => {
                server.release_reply_pending = false;
                if message.first() != Some(&b'I') {
                    server.mark_bad("release_query left the backend inside a transaction");
                    return Err(Error::ProtocolSyncError(
                        "release_query left the backend inside a transaction".to_string(),
                    ));
                }
            }
            b'E' => {
                server.release_reply_pending = false;
                server.release_failed = true;
                let summary = match PgErrorMsg::parse(&message) {
                    Ok(msg) => format!("SQLSTATE {}: {}", msg.code, sanitize_for_log(&msg.message)),
                    Err(_) => "unparseable ErrorResponse".to_string(),
                };
                server.mark_bad(&format!(
                    "release_query failed, the following exchange was skipped: {summary}"
                ));
                return Err(Error::ReleaseQueryFailed(summary));
            }
            other => {
                server.release_reply_pending = false;
                let reason = format!(
                    "unexpected message '{}' in the release_query reply",
                    other as char
                );
                server.mark_bad(&reason);
                return Err(Error::ProtocolSyncError(reason));
            }
        }
    }
    server.release_reply_resets_session = false;
    if std::mem::take(&mut server.release_reply_resets_all) {
        server
            .server_parameters
            .remove_startup_only_params_after_session_reset();
    }
    server.touch_activity();
    Ok(())
}

/// What a client is told about an exchange PostgreSQL skipped because the
/// release query before it failed.
pub(crate) const SKIPPED_EXCHANGE_MESSAGE: &str =
    "server connection cleanup after the previous transaction failed; the query was not executed";

/// The answer to an exchange PostgreSQL skipped because the release query
/// before it failed (`Error::ReleaseQueryFailed`): an ERROR, and
/// ReadyForQuery unless the exchange ended in Flush, whose Sync the client
/// still sends and PostgreSQL answers. The query was not executed and the
/// client is not in a transaction; the backend is already marked bad.
pub(crate) fn skipped_exchange_reply(server: &mut Server) -> BytesMut {
    let mut reply = crate::messages::nonfatal_error_message(SKIPPED_EXCHANGE_MESSAGE, "08006");
    server.response_cycle_had_error = true;
    server.in_copy_mode = false;
    if server.is_async() {
        server.reset_expected_responses();
        server.data_available = false;
    } else {
        let ready = crate::messages::ready_for_query(false);
        let mut state = BytesMut::from(&ready[5..]);
        if let Err(err) = handle_ready_for_query(server, &mut state) {
            error!("idle ReadyForQuery was rejected: {err}");
        }
        reply.put(ready);
    }
    reply
}

/// Receive data from the server in response to a client request.
/// Must be called multiple times while `server.is_data_available()` is true.
pub(crate) async fn recv<C, const ONE_MESSAGE: bool>(
    server: &mut Server,
    mut client_stream: C,
    mut client_server_parameters: Option<&mut ServerParameters>,
    defer_large_messages: bool,
) -> Result<BytesMut, Error>
where
    C: tokio::io::AsyncWrite + std::marker::Unpin,
{
    if server.release_reply_pending {
        settle_release_reply(server).await?;
    }
    let idle_timeout = if ONE_MESSAGE {
        crate::config::proxy_copy_data_timeout()
    } else {
        Duration::ZERO
    };
    // Handle deferred large message from previous recv() call.
    // When recv() encounters a large backend message but the buffer already has
    // accumulated messages, it returns the buffer first (for response ordering)
    // and saves the large message header here for the next call.
    if let Some((code_u8, message_len)) = server.pending_large_message {
        let result = match code_u8 as char {
            'D' => handle_large_data_row(server, &mut client_stream, code_u8, message_len).await,
            'd' => handle_large_copy_data(server, &mut client_stream, code_u8, message_len).await,
            'V' => {
                handle_large_function_call_response(
                    server,
                    &mut client_stream,
                    code_u8,
                    message_len,
                )
                .await
            }
            _ => unreachable!("pending_large_message should only contain 'D', 'd', or 'V'"),
        };
        // Clear the deferred header once the frame was read whole: after
        // success, and after a client gone mid-frame, whose rest was still
        // read. A backend failure leaves it, and the backend is closed.
        if matches!(result, Ok(_) | Err(Error::ClientGoneMidStream(_))) {
            server.pending_large_message = None;
        }
        return result;
    }

    // this path called
    // `stats.wait_reading()` at the top of every loop iter and
    // `stats.wait_idle()` after every successful body read. On a 100-row
    // SELECT that is 200 atomic load+store pairs on the shared
    // `state_wait` field just to flip a nibble that observers only sample
    // at admin-poll cadence. Hoist `wait_reading` once before the loop
    // and let an RAII guard restore `wait_idle` on every exit
    // (break, return Ok/Err, panic, cancel) so the per-message in-loop
    // touches disappear without changing the observable steady-state
    // (sampler sees "reading" while a recv loop is in flight, "idle"
    // once it has returned to the caller).
    server.stats.wait_reading();
    let _wait_guard = WaitIdleOnDrop(std::sync::Arc::clone(&server.stats));
    // Snapshot of `general.response_flush_threshold` taken when this backend
    // was created: how many bytes of the response we batch before handing it
    // to the client. Read once instead of per message - it cannot change
    // mid-relay - and it keeps the hand-off below borrowing `server.buffer`
    // mutably while still reading the field.
    let flush_threshold = server.response_flush_threshold;
    loop {
        // In async mode, check if all expected responses have been received
        if !ONE_MESSAGE && server.is_async() && server.expected_responses() == 0 {
            server.data_available = false;
            break;
        }

        let (code_u8, message_len) = if ONE_MESSAGE {
            read_message_header(&mut crate::messages::socket::ReadIdleTimeout::new(
                &mut *server.stream,
                idle_timeout,
            ))
            .await?
        } else {
            read_message_header(&mut *server.stream).await?
        };
        // Handle large DataRow messages that exceed max_message_size
        if server.max_message_size > 0
            && message_len > server.max_message_size
            && code_u8 as char == 'D'
        {
            // Return buffered responses before direct streaming. A client
            // with pending synthetic acknowledgements also needs this handoff
            // when the large frame is the first backend response.
            if !server.buffer.is_empty() || defer_large_messages {
                server.pending_large_message = Some((code_u8, message_len));
                server.data_available = true;
                // zero-copy split - hands ownership of the filled
                // bytes without alloc+memcpy; leaves capacity behind for
                // the next round.
                let result = server.buffer.split();
                server.stats.data_received(result.len());
                server.touch_activity();
                return Ok(result);
            }
            return handle_large_data_row(server, &mut client_stream, code_u8, message_len).await;
        }

        // Handle large CopyData messages that exceed max_message_size
        if server.max_message_size > 0
            && message_len > server.max_message_size
            && code_u8 as char == 'd'
        {
            if !server.buffer.is_empty() || defer_large_messages {
                server.pending_large_message = Some((code_u8, message_len));
                server.data_available = true;
                // zero-copy split.
                let result = server.buffer.split();
                server.stats.data_received(result.len());
                server.touch_activity();
                return Ok(result);
            }
            return handle_large_copy_data(server, &mut client_stream, code_u8, message_len).await;
        }

        // Handle large FunctionCallResponse messages that exceed max_message_size
        if server.max_message_size > 0
            && message_len > server.max_message_size
            && code_u8 as char == 'V'
        {
            if !server.buffer.is_empty() || defer_large_messages {
                server.pending_large_message = Some((code_u8, message_len));
                server.data_available = true;
                // zero-copy split.
                let result = server.buffer.split();
                server.stats.data_received(result.len());
                server.touch_activity();
                return Ok(result);
            }
            return handle_large_function_call_response(
                server,
                &mut client_stream,
                code_u8,
                message_len,
            )
            .await;
        }

        // `>=` (not strict `>`) - same rationale as `messages::socket::*`:
        // a message claiming EXACTLY `MAX_MESSAGE_SIZE` (256MB) is treated
        // as malformed and rejected before allocation.
        if message_len >= MAX_MESSAGE_SIZE {
            error!(
                "[{}@{}] message size limit exceeded pid={}: received={} bytes, max={} bytes",
                server.address.username,
                server.address.pool_name,
                server.get_process_id(),
                message_len,
                MAX_MESSAGE_SIZE,
            );
            server.mark_bad(
                format!(
                    "Message size limit exceeded: {message_len} bytes (max: {MAX_MESSAGE_SIZE} bytes)"
                )
                .as_str(),
            );
            return Err(MaxMessageSize);
        }

        // Row and COPY payloads need no inspection. Read them directly into
        // the response batch, avoiding a temporary split and a second copy
        // for every row. The large-frame streaming paths above are unchanged.
        if matches!(code_u8, b'D' | b'd') {
            let result = if ONE_MESSAGE {
                read_message_body_append(
                    &mut crate::messages::socket::ReadIdleTimeout::new(
                        &mut *server.stream,
                        idle_timeout,
                    ),
                    &mut server.buffer,
                    code_u8,
                    message_len,
                )
                .await
            } else {
                read_message_body_append(
                    &mut *server.stream,
                    &mut server.buffer,
                    code_u8,
                    message_len,
                )
                .await
            };
            if let Err(err) = result {
                error!(
                    "[{}@{}] server connection terminated pid={}: {err}",
                    server.address.username,
                    server.address.pool_name,
                    server.get_process_id(),
                );
                server.mark_bad(format!("Failed to read message data: {err}").as_str());
                return Err(err);
            }
            if code_u8 == b'D' {
                server.data_available = true;
            }
            if ONE_MESSAGE || server.buffer.len() >= flush_threshold {
                break;
            }
            continue;
        }

        // Read body into per-connection reusable buffer (header already consumed above).
        // per-iter `stats.wait_idle()` dropped - the
        // `WaitIdleOnDrop` guard above restores idle once the recv
        // loop returns to the caller. Observers only ever sampled
        // the nibble at admin-poll cadence; the transient
        // "idle-between-reads" flicker was pure observability noise.
        let result = if ONE_MESSAGE {
            read_message_body_reuse(
                &mut crate::messages::socket::ReadIdleTimeout::new(
                    &mut *server.stream,
                    idle_timeout,
                ),
                &mut server.read_buf,
                code_u8,
                message_len,
            )
            .await
        } else {
            read_message_body_reuse(
                &mut *server.stream,
                &mut server.read_buf,
                code_u8,
                message_len,
            )
            .await
        };
        let mut message = match result {
            Ok(message) => message,
            Err(err) => {
                error!(
                    "[{}@{}] server connection terminated pid={}: {err}",
                    server.address.username,
                    server.address.pool_name,
                    server.get_process_id(),
                );
                server.mark_bad(format!("Failed to read message data: {err}").as_str());
                return Err(err);
            }
        };

        if code_u8 == b'W' {
            error!(
                "[{}@{}] unsupported CopyBothResponse pid={}: COPY BOTH requires a full-duplex replication relay",
                server.address.username,
                server.address.pool_name,
                server.get_process_id(),
            );
            server.mark_bad("unsupported CopyBothResponse");
            return Err(Error::ProtocolSyncError(
                "COPY BOTH is not supported by pg_doorman".to_string(),
            ));
        }

        // Buffer the message we'll forward to the client later.
        server.buffer.put(&message[..]);

        let code = message.get_u8() as char;
        let _len = message.get_i32();

        match code {
            // ReadyForQuery - server is ready for a new query
            'Z' => {
                // After a failed release the aborted block is the release's
                // own; the client never opened one.
                if server.release_failed && message.first() == Some(&b'E') {
                    let status = server.buffer.len() - 1;
                    server.buffer[status] = b'I';
                    message = BytesMut::from(&b"I"[..]);
                }
                handle_ready_for_query(server, &mut message)?;
                break;
            }

            // ErrorResponse - server encountered an error
            'E' => {
                handle_error_response(server, &mut message);
                // In async mode, error aborts remaining operations in pipeline
                if server.is_async() {
                    server.reset_expected_responses();
                }
            }

            // CommandComplete - command executed successfully
            'C' => {
                handle_command_complete(server, &message);
                // In async mode, this ends an Execute operation
                if server.is_async() {
                    server.decrement_expected();
                }
            }

            // ParameterStatus - server parameter changed
            'S' => {
                handle_parameter_status(server, &mut message, &mut client_server_parameters)?;
            }

            // CopyInResponse: copy is starting from client to server.
            'G' => {
                server.in_copy_mode = true;
                // The next bytes belong to the client even if a preceding
                // statement in this Simple Query produced DataRows.
                server.data_available = false;
                // CopyXResponse is the terminal
                // response to an Execute. In async (Flush-only)
                // mode `expected_responses` must be decremented
                // here - without this, the recv loop's "no more
                // responses expected" guard at the loop top stays
                // armed (≥1) and a subsequent recv blocks waiting
                // for backend bytes that will never come (the
                // next bytes belong to the CLIENT direction in
                // COPY mode). Symptom: hang until network/idle
                // timeout. Mirrors how 'C' / 'I' decrement for
                // non-COPY Executes.
                if server.is_async() {
                    server.decrement_expected();
                }
                break;
            }

            // CopyOutResponse: copy is starting from the server to the client.
            'H' => {
                server.in_copy_mode = true;
                server.data_available = true;
                // Do NOT decrement the async Execute response on
                // CopyOutResponse. Unlike CopyInResponse, COPY OUT still has
                // backend-to-client CopyData / CopyDone / CommandComplete
                // frames to relay. The Execute is complete only when the
                // terminal CommandComplete arrives; decrementing here made the
                // next recv() short-circuit on expected_responses == 0 and
                // left CopyData unread on the backend socket.
                break;
            }

            // CopyDone
            // Buffer until ReadyForQuery shows up, so don't exit the loop yet.
            'c' => (),

            // ParseComplete
            // Response to Parse message in extended query protocol.
            // Confirms the head of `registering_prepared_statement` was
            // accepted by PostgreSQL: drop it from the pending list so a
            // later ErrorResponse only rolls back the still-unconfirmed
            // names. Without this pop, an error on Parse #N rolled back
            // every Parse in the batch — even the ones PG had already
            // ParseComplete'd — which Java pgjdbc surfaced as
            // "Connection reset by peer" on its eighth pipelined batch.
            '1' => {
                let suppress_complete = server
                    .registering_prepared_statement
                    .pop_front()
                    .is_some_and(|pending| pending.suppress_complete);
                if suppress_complete {
                    // This Parse was inserted before a cold Bind/Describe.
                    // It is not a frontend operation and must neither leak a
                    // ParseComplete nor consume that operation's Flush reply.
                    server.buffer.truncate(server.buffer.len() - 5);
                    server.stats.data_received(5);
                } else if server.is_async() {
                    server.decrement_expected();
                }
            }

            // BindComplete
            // Response to Bind message in extended query protocol
            '2' => {
                if server.is_async() {
                    server.decrement_expected();
                }
            }

            // CloseComplete
            // Response to Close message in extended query protocol
            '3' => {
                if server.is_async() {
                    server.decrement_expected();
                }
            }

            // ParameterDescription
            // Response to Describe message for a statement
            't' => {
                if server.is_async() {
                    server.decrement_expected();
                }
            }

            // PortalSuspended
            // Indicates that Execute completed but portal still has rows
            's' => {
                if server.is_async() {
                    server.decrement_expected();
                }
            }

            // NoData
            // Response to Describe when statement/portal produces no rows
            // https://www.postgresql.org/docs/current/protocol-flow.html
            'n' => {
                if server.is_async() {
                    server.decrement_expected_describe_terminal();
                }
            }

            // FunctionCallResponse
            // Response to FunctionCall (F). The message body is opaque binary
            // function output and transaction state is reported by the later
            // ReadyForQuery message.
            'V' => {}

            // RowDescription
            // Response to Describe, or the first frame of an Execute SELECT.
            'T' => {
                if server.is_async() {
                    server.decrement_expected_describe_terminal();
                }
            }

            // EmptyQueryResponse
            // Response to Execute with an empty query string
            'I' => {
                if server.is_async() {
                    server.decrement_expected();
                }
            }

            // Anything else, e.g. notices, etc.
            // Keep buffering until ReadyForQuery shows up.
            _ => (),
        };

        // Idle notifications and COPY errors need no subsequent client request
        // or ReadyForQuery. Return the complete frame without waiting for one.
        if ONE_MESSAGE {
            break;
        }
    }

    // zero-copy hand-off. `BytesMut::clone()` would deep-copy every
    // byte of the response; `split()` takes ownership of the filled bytes
    // and leaves the capacity behind for the next call - same effect as
    // clone+clear without the alloc+memcpy. At 100k qps with multi-KiB
    // responses this saves hundreds of MB/s of allocator + memcpy work.
    let bytes = if server.buffer.len() > flush_threshold {
        // Buffer outgrew the configured per-server cap. Take the whole
        // payload and hand the connection a fresh one - bounds the long-tail
        // memory of a chatty backend.
        //
        // The replacement gets twice the threshold because the flush check runs
        // AFTER a message is appended: a bulk response always ends up past the
        // threshold, so sizing the replacement at exactly the threshold would
        // guarantee a realloc plus a full memcpy of the accumulated bytes on
        // the very next cycle. On a 20 MB report that is thousands of avoidable
        // reallocations; the cost is one extra threshold worth of steady state
        // on a backend that has already served an oversized response.
        std::mem::replace(
            &mut server.buffer,
            BytesMut::with_capacity(flush_threshold * 2),
        )
    } else {
        // Hot path: O(1) split - no allocation, no memcpy.
        server.buffer.split()
    };

    // Keep track of how much data we got from the server for stats.
    server.stats.data_received(bytes.len());

    // Successfully received data from server
    server.touch_activity();

    // Pass the data back to the client.
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    //! Pure-function tests for CommandComplete tag classification.
    //!
    //! The tag strings were captured empirically against PostgreSQL 16 by
    //! connecting with `psql` and inspecting the CommandComplete payload —
    //! PostgreSQL does not expose the tag list as a public contract, so these
    //! tests pin the bytes pg_doorman relies on.
    //!
    //! Note the two non-obvious cases:
    //! * `RESET ALL` is reported as `RESET\0`, not `RESET ALL\0`.
    //! * `CLOSE ALL` is reported as `CLOSE CURSOR ALL\0`, not `CLOSE ALL\0`.

    use super::{
        classify_command_complete, classify_command_complete_with_reset_attribution,
        handle_command_complete, handle_error_response, handle_ready_for_query,
        CommandCompleteEffect,
    };
    use crate::client::util::extract_set_cleanup_commands;
    use crate::errors::Error;
    use crate::server::cleanup::ResetCleanupCommand;
    use ahash::RandomState;
    use bytes::{BufMut, BytesMut};
    use lru::LruCache;
    use std::num::NonZeroUsize;

    #[tokio::test]
    async fn error_response_exports_rejected_pending_parse_names() {
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(16).unwrap(),
            RandomState::new(),
        ));
        server
            .prepared_statement_cache
            .as_mut()
            .unwrap()
            .put("DOORMAN_bad".to_string(), ());
        server.registering_prepared_statement.push_back(
            super::super::server_backend::PendingPreparedStatement {
                name: "DOORMAN_bad".to_string(),
                suppress_complete: false,
            },
        );

        let mut body = BytesMut::from(&b"SERROR\0C42601\0Mbad parse\0\0"[..]);
        handle_error_response(&mut server, &mut body);

        assert!(server.registering_prepared_statement.is_empty());
        assert_eq!(
            server.take_rejected_prepared_statement_names(),
            vec!["DOORMAN_bad".to_string()]
        );
        assert!(
            !server.has_prepared_statement("DOORMAN_bad"),
            "server-side optimistic prepared cache entry must be rolled back too"
        );
    }

    /// A shared DOORMAN_N parsed before a result-shape DDL fails every Bind
    /// with 0A000, and a repeated client Parse is skipped while the backend
    /// holds that name. DEALLOCATE ALL at checkin limits this to one error per
    /// backend; without it the stale plan keeps failing until LRU eviction.
    #[tokio::test]
    async fn cached_plan_result_type_error_arms_prepared_cleanup() {
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(16).unwrap(),
            RandomState::new(),
        ));
        server
            .prepared_statement_cache
            .as_mut()
            .unwrap()
            .put("DOORMAN_1".to_string(), ());
        assert!(!server.cleanup_state.needs_cleanup_prepare);

        let mut body =
            BytesMut::from(&b"SERROR\0C0A000\0Mcached plan must not change result type\0\0"[..]);
        handle_error_response(&mut server, &mut body);

        assert!(
            server.cleanup_state.needs_cleanup_prepare,
            "0A000 must schedule DEALLOCATE ALL so the next Parse reaches PostgreSQL"
        );
    }

    /// A reset in the middle of a pipeline drops the statements confirmed
    /// before it, but the Parses that follow it still reach PostgreSQL:
    /// forgetting them makes the next Parse of the same alias hit 42P05.
    #[tokio::test]
    async fn reset_keeps_registrations_of_later_parses() {
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        let mut cache = LruCache::with_hasher(NonZeroUsize::new(16).unwrap(), RandomState::new());
        cache.put("DOORMAN_old".to_string(), ());
        cache.put("DOORMAN_later".to_string(), ());
        server.prepared_statement_cache = Some(cache);
        server.registering_prepared_statement.push_back(
            super::super::server_backend::PendingPreparedStatement {
                name: "DOORMAN_later".to_string(),
                suppress_complete: false,
            },
        );

        handle_command_complete(&mut server, &BytesMut::from(&b"DEALLOCATE ALL\0"[..]));

        assert!(!server.has_prepared_statement("DOORMAN_old"));
        assert!(
            server.has_prepared_statement("DOORMAN_later"),
            "a Parse after the reset still creates its statement"
        );
        assert_eq!(server.registering_prepared_statement.len(), 1);
    }

    /// SQL-level PREPARE keeps a transaction-pool client on its backend only
    /// while one of its statements exists there; the count follows the
    /// CommandComplete tags PostgreSQL returns.
    #[tokio::test]
    async fn sql_prepare_and_deallocate_tags_count_backend_statements() {
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        for (tag, expected) in [
            (&b"PREPARE\0"[..], 1),
            (&b"PREPARE\0"[..], 2),
            (&b"DEALLOCATE\0"[..], 1),
            (&b"DEALLOCATE ALL\0"[..], 0),
            (&b"PREPARE\0"[..], 1),
            (&b"DISCARD ALL\0"[..], 0),
            (&b"DEALLOCATE\0"[..], 0),
        ] {
            handle_command_complete(&mut server, &BytesMut::from(tag));
            assert_eq!(
                server.cleanup_state.sql_prepared_statements,
                expected,
                "after {}",
                String::from_utf8_lossy(tag)
            );
        }
    }

    /// DEALLOCATE ALL drops every shared DOORMAN_N on the backend, so only
    /// errors showing that the pooler's view of the backend's statements is
    /// stale may schedule it. An ordinary statement error leaves them valid.
    #[tokio::test]
    async fn only_stale_statement_errors_arm_prepared_cleanup() {
        for (sqlstate, arms) in [
            ("23505", false), // unique_violation
            ("40001", false), // serialization_failure
            ("22012", false), // division_by_zero
            ("57014", false), // query_canceled
            ("42P01", false), // undefined_table
            ("0A000", true),  // cached plan must not change result type
            ("26000", true),  // prepared statement does not exist
            ("42P05", true),  // prepared statement already exists
        ] {
            let (mut server, _peer) = crate::server::Server::test_silent_socket();
            server.prepared_statement_cache = Some(LruCache::with_hasher(
                NonZeroUsize::new(16).unwrap(),
                RandomState::new(),
            ));
            let body = format!("SERROR\0VERROR\0C{sqlstate}\0Mfailed\0\0");
            handle_error_response(&mut server, &mut BytesMut::from(body.as_bytes()));
            assert_eq!(
                server.cleanup_state.needs_cleanup_prepare, arms,
                "SQLSTATE {sqlstate}"
            );
        }
    }

    /// An ErrorResponse without a readable SQLSTATE keeps the conservative
    /// reset: nothing proves the statements are still valid.
    #[tokio::test]
    async fn unparseable_error_arms_prepared_cleanup() {
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(16).unwrap(),
            RandomState::new(),
        ));
        handle_error_response(&mut server, &mut BytesMut::from(&b"garbage"[..]));
        assert!(server.cleanup_state.needs_cleanup_prepare);
    }

    /// A PostgreSQL error can carry megabytes (a RAISE of a large value, a
    /// DETAIL quoting a long key); the log line keeps its start and says how
    /// much was left out.
    #[test]
    fn logged_error_text_is_bounded() {
        let logged = super::sanitize_for_log(&"y".repeat(2 * 1024 * 1024));
        assert!(logged.len() < 2048, "{} bytes logged", logged.len());
        assert!(logged.ends_with(&format!("... ({} more bytes)", 2 * 1024 * 1024 - 1024)));
        assert_eq!(super::sanitize_for_log("line\nnext"), "line\\nnext");
    }

    /// COPY data may take longer to reach a backend than the limit in total;
    /// the limit bounds a pause without progress, as the setting promises.
    #[tokio::test]
    async fn send_limit_bounds_a_pause_not_the_whole_transfer() {
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        let data = vec![b'd'; 4 * 1024 * 1024];
        let reader = tokio::spawn(async move {
            let mut chunk = vec![0_u8; 256 * 1024];
            let mut total = 0;
            while total < 4 * 1024 * 1024 {
                tokio::time::sleep(Duration::from_millis(30)).await;
                total += peer.read(&mut chunk).await.unwrap();
            }
            total
        });

        let sent =
            super::send_and_flush_timeout(&mut server, &data, Duration::from_millis(100)).await;

        assert!(sent.is_ok(), "{sent:?}");
        assert_eq!(reader.await.unwrap(), data.len());
        assert!(!server.is_bad());
    }

    /// PostgreSQL's reply to the release prefix of the default query.
    fn release_ok_reply() -> BytesMut {
        let mut reply = BytesMut::new();
        for (tag, row) in [("BEGIN", false), ("SELECT 1", true), ("COMMIT", false)] {
            reply.put(crate::messages::parse_complete());
            reply.put_slice(&[b'2', 0, 0, 0, 4]);
            if row {
                reply.put_slice(&[b'D', 0, 0, 0, 14, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
            }
            reply.put(crate::messages::command_complete(tag));
        }
        reply.put_slice(&[b'I', 0, 0, 0, 4]);
        reply.put(crate::messages::ready_for_query(false));
        reply
    }

    /// The release reply precedes the reply to the exchange sent after it
    /// and is read away; the caller gets only its own reply.
    #[tokio::test]
    async fn the_release_reply_is_read_before_the_next_reply() {
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.release_reply_pending = true;
        let mut own = crate::messages::command_complete("SELECT 1");
        own.put(crate::messages::ready_for_query(false));
        peer.write_all(&release_ok_reply()).await.unwrap();
        peer.write_all(&own).await.unwrap();

        let reply = server.recv(tokio::io::sink(), None).await.unwrap();

        assert_eq!(&reply[..], &own[..]);
        assert!(!server.release_reply_pending);
        assert!(!server.is_bad());
    }

    /// A failed release makes PostgreSQL skip the next exchange up to a
    /// Sync: nothing more is read, the exchange fails and the backend,
    /// whose session the release did not clean, is not reused.
    #[tokio::test]
    async fn a_failed_release_fails_the_skipped_exchange() {
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.release_reply_pending = true;
        let mut reply = BytesMut::from(&crate::messages::parse_complete()[..]);
        let mut error = BytesMut::new();
        error.put_slice(b"SERROR\0VERROR\0C57014\0Mcanceling statement due to user request\0\0");
        reply.put_u8(b'E');
        reply.put_i32(error.len() as i32 + 4);
        reply.put(error);
        peer.write_all(&reply).await.unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            server.recv(tokio::io::sink(), None),
        )
        .await
        .expect("the skipped exchange has no reply to wait for");

        match result {
            Err(Error::ReleaseQueryFailed(summary)) => {
                assert!(summary.contains("57014"), "{summary}")
            }
            other => panic!("expected ReleaseQueryFailed, got {other:?}"),
        }
        assert!(!server.release_reply_pending);
        assert!(server.is_bad());
    }

    /// After a failed release, the transaction block PostgreSQL reports as
    /// aborted at the client's next Sync is the release's own; the client
    /// never opened one, so it is told the session is idle.
    #[tokio::test]
    async fn the_ready_for_query_after_a_failed_release_reports_idle() {
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.release_reply_pending = true;
        let mut reply = BytesMut::from(&crate::messages::parse_complete()[..]);
        let mut error = BytesMut::new();
        error.put_slice(b"SERROR\0VERROR\0C57014\0Mcanceling statement due to user request\0\0");
        reply.put_u8(b'E');
        reply.put_i32(error.len() as i32 + 4);
        reply.put(error);
        peer.write_all(&reply).await.unwrap();
        assert!(server.recv(tokio::io::sink(), None).await.is_err());

        peer.write_all(&[b'Z', 0, 0, 0, 5, b'E']).await.unwrap();
        let ready = server.recv(tokio::io::sink(), None).await.unwrap();

        assert_eq!(&ready[..], &[b'Z', 0, 0, 0, 5, b'I']);
        assert!(!server.in_transaction());
    }

    /// A named Parse later in the same pipeline was forwarded before the
    /// client's DEALLOCATE ALL or DISCARD ALL was answered, so the reset does
    /// not prove the client has no protocol statements under its own names.
    #[tokio::test]
    async fn client_reset_keeps_the_named_protocol_statement_mark() {
        for tag in [&b"DEALLOCATE ALL\0"[..], &b"DISCARD ALL\0"[..]] {
            let (mut server, _peer) = crate::server::Server::test_silent_socket();
            server.cleanup_state.client_named_protocol_statements = true;

            handle_command_complete(&mut server, &BytesMut::from(tag));

            assert!(
                server.cleanup_state.client_named_protocol_statements,
                "{}",
                String::from_utf8_lossy(tag)
            );
        }
    }

    /// A statement parsed under a client name after the reset in the same
    /// pipeline still exists, so the pooler's DEALLOCATE ALL stays armed.
    #[tokio::test]
    async fn client_reset_leaves_later_named_statements_to_checkin_cleanup() {
        for tag in [&b"DEALLOCATE ALL\0"[..], &b"DISCARD ALL\0"[..]] {
            let (mut server, _peer) = crate::server::Server::test_silent_socket();
            server.mark_dirty();
            server.cleanup_state.client_named_protocol_statements = true;

            handle_command_complete(&mut server, &BytesMut::from(tag));

            assert!(
                server.cleanup_state.needs_cleanup_prepare,
                "{}",
                String::from_utf8_lossy(tag)
            );
        }
    }

    /// The error kept for housekeeping queries ends up in their errors and
    /// in the log lines built from them, so it is bounded the same way.
    #[tokio::test]
    async fn kept_sql_error_text_is_bounded() {
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        let body = format!(
            "SERROR\0VERROR\0CP0001\0M{}\0\0",
            "y".repeat(2 * 1024 * 1024)
        );
        handle_error_response(&mut server, &mut BytesMut::from(body.as_bytes()));
        let (sqlstate, message) = server.last_sql_error.take().expect("SQL error kept");
        assert_eq!(sqlstate, "P0001");
        assert!(message.len() < 2048, "{} bytes kept", message.len());
    }

    /// A missing `DOORMAN_missing_*` statement is one the pooler named on
    /// purpose; only a missing statement it believed present shows that its
    /// view of the backend is stale.
    #[tokio::test]
    async fn only_a_missing_known_statement_schedules_statement_reset() {
        for (name, arms) in [("DOORMAN_missing_7", false), ("DOORMAN_7", true)] {
            let (mut server, _peer) = crate::server::Server::test_silent_socket();
            server.prepared_statement_cache = Some(LruCache::with_hasher(
                NonZeroUsize::new(16).unwrap(),
                RandomState::new(),
            ));
            let body = format!(
                "SERROR\0VERROR\0C26000\0Mprepared statement \"{name}\" does not exist\0\0"
            );
            handle_error_response(&mut server, &mut BytesMut::from(body.as_bytes()));
            assert_eq!(server.cleanup_state.needs_cleanup_prepare, arms, "{name}");
        }
    }

    /// A DEALLOCATE ALL bound earlier in the batch drops every statement when
    /// PostgreSQL runs it, so while the batch is assembled only statements
    /// prepared after it count as present; once it ran, the cache holds just
    /// those.
    #[tokio::test]
    async fn a_queued_statements_reset_hides_statements_prepared_before_it() {
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(16).unwrap(),
            RandomState::new(),
        ));
        let parse = crate::messages::Parse::from_parts("SELECT 1", &[]);
        assert!(server
            .prepare_statement_for_frontend(&parse, "DOORMAN_warm")
            .unwrap()
            .is_some());
        server.registering_prepared_statement.clear();
        assert!(server.has_prepared_statement("DOORMAN_warm"));

        server.queue_statements_reset();
        assert!(
            !server.has_prepared_statement("DOORMAN_warm"),
            "the reset drops it before the rest of the batch runs"
        );
        assert!(server
            .prepare_statement_for_frontend(&parse, "DOORMAN_warm")
            .unwrap()
            .is_some());
        assert!(server.has_prepared_statement("DOORMAN_warm"));

        handle_command_complete(&mut server, &BytesMut::from(&b"DEALLOCATE ALL\0"[..]));
        assert!(
            server.has_prepared_statement("DOORMAN_warm"),
            "prepared again after the reset"
        );
        server.queue_statements_reset();
        let mut ready = BytesMut::from(&b"I"[..]);
        handle_ready_for_query(&mut server, &mut ready).unwrap();
        assert!(
            server.has_prepared_statement("DOORMAN_warm"),
            "a batch that ended without running the reset changed nothing"
        );
    }

    /// An error in a Flush pipeline arms the SET/RESET cleanup, whose
    /// attribution the skipped suffix breaks, but prepared statements stay
    /// valid unless the error itself says otherwise.
    #[tokio::test]
    async fn async_statement_error_keeps_prepared_statements() {
        for (sqlstate, arms) in [("22012", false), ("0A000", true)] {
            let (mut server, _peer) = crate::server::Server::test_silent_socket();
            server.prepared_statement_cache = Some(LruCache::with_hasher(
                NonZeroUsize::new(16).unwrap(),
                RandomState::new(),
            ));
            server.set_async_mode(true);
            let body = format!("SERROR\0VERROR\0C{sqlstate}\0Mfailed\0\0");
            handle_error_response(&mut server, &mut BytesMut::from(body.as_bytes()));
            assert!(
                server.cleanup_state.needs_cleanup_set,
                "SQLSTATE {sqlstate}"
            );
            assert_eq!(
                server.cleanup_state.needs_cleanup_prepare, arms,
                "SQLSTATE {sqlstate}"
            );
        }
    }

    #[tokio::test]
    async fn async_sql_error_keeps_backend_owned_until_sync_but_fatal_error_evicts() {
        for (severity, bad) in [("ERROR", false), ("FATAL", true)] {
            let (mut server, _peer) = crate::server::Server::test_silent_socket();
            server.session_mode = false;
            server.set_async_mode(true);
            let body = format!("S{severity}\0V{severity}\0C57014\0MCOPY canceled\0\0");
            handle_error_response(&mut server, &mut BytesMut::from(body.as_bytes()));
            assert_eq!(
                server.is_bad(),
                bad,
                "unexpected disposition for {severity}"
            );
            assert!(server.is_async(), "an error is not a Sync acknowledgement");
            assert!(server.response_cycle_had_error);
            assert!(!server.is_data_available());
        }
    }

    #[tokio::test]
    async fn malformed_parameter_status_fails_before_returning_buffered_bytes() {
        use crate::server::{Server, ServerParameters};
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        peer.write_all(&[
            b'S', 0, 0, 0, 7, b'b', b'a', b'd', // malformed key without NUL
            b'Z', 0, 0, 0, 5, b'I', // ReadyForQuery must not make recv return buffered bytes
        ])
        .await
        .expect("peer must write malformed ParameterStatus sequence");

        let mut client_params = ServerParameters::new();
        let err = server
            .recv(&mut tokio::io::sink(), Some(&mut client_params))
            .await
            .expect_err("malformed ParameterStatus must fail before bytes are returned");

        assert!(
            server.is_bad(),
            "malformed ParameterStatus must evict the backend"
        );
        assert!(
            err.to_string().contains("ParameterStatus"),
            "error should identify malformed ParameterStatus, got {err}"
        );
    }

    #[test]
    fn set_tag_arms_set_cleanup() {
        assert_eq!(
            classify_command_complete(b"SET\0"),
            CommandCompleteEffect::ArmSet,
        );
    }

    #[test]
    fn reset_tag_keeps_set_cleanup_armed() {
        // PostgreSQL emits the same `RESET\0` tag for `RESET ALL` and
        // `RESET foo.bar`. Because a per-GUC reset can leave other dirty GUCs
        // such as `client.app_user` behind, pg_doorman must fail closed and
        // keep the checkin-time `RESET ALL` armed.
        assert_eq!(
            classify_command_complete(b"RESET\0"),
            CommandCompleteEffect::None,
        );
    }

    #[test]
    fn reset_all_attribution_disarms_set_cleanup() {
        assert_eq!(
            classify_command_complete_with_reset_attribution(
                b"RESET\0",
                Some(ResetCleanupCommand::ResetAll),
            ),
            CommandCompleteEffect::DisarmSet,
        );
        assert_eq!(
            classify_command_complete_with_reset_attribution(
                b"RESET\0",
                Some(ResetCleanupCommand::PerGucReset),
            ),
            CommandCompleteEffect::None,
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reset_all_disarms_only_after_successful_idle_ready_for_query() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server.cleanup_state.needs_cleanup_set = true;
        server.cleanup_state.needs_cleanup_session_authorization = true;
        server
            .server_parameters
            .set_param("client.app_user", "alice", true);
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetAll]);

        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_set,
            "RESET ALL must remain pending until ReadyForQuery confirms the implicit transaction"
        );
        assert!(
            server
                .server_parameters
                .as_hashmap()
                .contains_key("client.app_user"),
            "startup-only mirrors must remain intact before transaction outcome"
        );
        assert!(
            server.cleanup_state.needs_cleanup_session_authorization,
            "RESET ALL must not prove that SET SESSION AUTHORIZATION was reset"
        );

        handle_ready_for_query(&mut server, &mut BytesMut::from(&b"I"[..]))
            .expect("valid idle ReadyForQuery");

        assert!(
            !server.cleanup_state.needs_cleanup_set,
            "successful implicit transaction should commit RESET ALL disarm"
        );
        assert!(
            !server
                .server_parameters
                .as_hashmap()
                .contains_key("client.app_user"),
            "committed RESET ALL should invalidate startup-only mirrors"
        );
        assert!(server.cleanup_state.needs_cleanup_session_authorization);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn error_after_reset_all_keeps_cleanup_and_parameter_mirror() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server.cleanup_state.needs_cleanup_set = true;
        server
            .server_parameters
            .set_param("client.app_user", "alice", true);
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetAll]);

        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));
        handle_error_response(
            &mut server,
            &mut BytesMut::from(&b"SERROR\0C22012\0Mdivision by zero\0\0"[..]),
        );
        handle_ready_for_query(&mut server, &mut BytesMut::from(&b"I"[..]))
            .expect("valid idle ReadyForQuery");

        assert!(
            server.cleanup_state.needs_cleanup_set,
            "a later error rolls back RESET ALL and must keep cleanup armed"
        );
        assert!(
            server
                .server_parameters
                .as_hashmap()
                .contains_key("client.app_user"),
            "a rolled-back RESET ALL must not invalidate the backend mirror"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn set_after_reset_all_keeps_cleanup_but_invalidates_old_startup_mirror() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server.cleanup_state.needs_cleanup_set = true;
        server
            .server_parameters
            .set_param("search_path", "tenant_a", true);
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetAll]);
        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));

        server.track_set_cleanup_commands(extract_set_cleanup_commands(
            b"SET statement_timeout = 1000",
        ));
        handle_command_complete(&mut server, &BytesMut::from(&b"SET\0"[..]));
        handle_ready_for_query(&mut server, &mut BytesMut::from(&b"I"[..]))
            .expect("valid idle ReadyForQuery");

        assert!(
            server.cleanup_state.needs_cleanup_set,
            "a SET after RESET ALL must keep check-in cleanup armed"
        );
        assert!(
            !server
                .server_parameters
                .as_hashmap()
                .contains_key("search_path"),
            "committed RESET ALL must invalidate mirrors that predate a later SET"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_disarms_follow_ready_for_query_outcome_and_command_order() {
        use crate::server::Server;

        let mut committed = Server::test_dead_socket();
        committed.cleanup_state.needs_cleanup_role = true;
        committed.cleanup_state.needs_cleanup_session_authorization = true;
        committed.track_reset_cleanup_commands([ResetCleanupCommand::ResetSessionAuthorization]);
        handle_command_complete(&mut committed, &BytesMut::from(&b"RESET\0"[..]));

        assert!(committed.cleanup_state.needs_cleanup_role);
        assert!(committed.cleanup_state.needs_cleanup_session_authorization);

        committed
            .track_set_cleanup_commands(extract_set_cleanup_commands(b"SET ROLE audit_reader"));
        handle_command_complete(&mut committed, &BytesMut::from(&b"SET\0"[..]));
        handle_ready_for_query(&mut committed, &mut BytesMut::from(&b"I"[..]))
            .expect("valid idle ReadyForQuery");

        assert!(
            committed.cleanup_state.needs_cleanup_role,
            "SET ROLE after identity reset must keep role cleanup armed"
        );
        assert!(
            !committed.cleanup_state.needs_cleanup_session_authorization,
            "successful identity reset should disarm session authorization cleanup"
        );

        let mut rolled_back = Server::test_dead_socket();
        rolled_back.cleanup_state.needs_cleanup_role = true;
        rolled_back
            .cleanup_state
            .needs_cleanup_session_authorization = true;
        rolled_back.track_reset_cleanup_commands([ResetCleanupCommand::ResetSessionAuthorization]);
        handle_command_complete(&mut rolled_back, &BytesMut::from(&b"RESET\0"[..]));
        handle_error_response(
            &mut rolled_back,
            &mut BytesMut::from(&b"SERROR\0C22012\0Mdivision by zero\0\0"[..]),
        );
        handle_ready_for_query(&mut rolled_back, &mut BytesMut::from(&b"I"[..]))
            .expect("valid idle ReadyForQuery");

        assert!(rolled_back.cleanup_state.needs_cleanup_role);
        assert!(
            rolled_back
                .cleanup_state
                .needs_cleanup_session_authorization
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reset_all_inside_transaction_keeps_set_cleanup_armed() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server.in_transaction = true;
        server.cleanup_state.needs_cleanup_set = true;
        server
            .server_parameters
            .set_param("client.app_user", "alice", true);
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetAll]);

        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_set,
            "RESET ALL inside a transaction must not disarm cleanup before \
             ReadyForQuery proves the transaction committed"
        );
        assert!(
            server
                .server_parameters
                .as_hashmap()
                .contains_key("client.app_user"),
            "startup-only GUC mirrors must not be invalidated while a later \
             rollback can restore the dirty server value"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn same_batch_begin_keeps_reset_disarms_armed_until_ready_for_query() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server.cleanup_state.needs_cleanup_set = true;
        server
            .server_parameters
            .set_param("client.app_user", "alice", true);

        handle_command_complete(&mut server, &BytesMut::from(&b"BEGIN\0"[..]));
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetAll]);
        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_set,
            "RESET ALL after BEGIN in the same simple-query batch must not \
             disarm cleanup before ReadyForQuery proves the transaction outcome"
        );
        assert!(
            server
                .server_parameters
                .as_hashmap()
                .contains_key("client.app_user"),
            "startup-only mirrors must not be invalidated by a RESET that can \
             still be rolled back later in the same simple-query batch"
        );

        let mut server = Server::test_dead_socket();
        server.cleanup_state.needs_cleanup_role = true;

        handle_command_complete(&mut server, &BytesMut::from(&b"BEGIN\0"[..]));
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetRole]);
        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_role,
            "RESET ROLE after BEGIN in the same simple-query batch must not \
             disarm role cleanup before the transaction outcome is known"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_disarms_inside_transaction_keep_cleanup_armed() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server.in_transaction = true;
        server.cleanup_state.needs_cleanup_role = true;
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetRole]);

        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_role,
            "RESET ROLE inside a transaction must not disarm cleanup before \
             ReadyForQuery proves the transaction committed"
        );

        let mut server = Server::test_dead_socket();
        server.in_transaction = true;
        server.cleanup_state.needs_cleanup_role = true;
        server.track_set_cleanup_commands(extract_set_cleanup_commands(b"SET ROLE DEFAULT"));

        handle_command_complete(&mut server, &BytesMut::from(&b"SET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_role,
            "SET ROLE DEFAULT inside a transaction must not disarm cleanup \
             before ReadyForQuery proves the transaction committed"
        );

        let mut server = Server::test_dead_socket();
        server.in_transaction = true;
        server.cleanup_state.needs_cleanup_role = true;
        server.cleanup_state.needs_cleanup_session_authorization = true;
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetSessionAuthorization]);

        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_session_authorization,
            "RESET SESSION AUTHORIZATION inside a transaction must not disarm \
             cleanup before ReadyForQuery proves the transaction committed"
        );
        assert!(
            server.cleanup_state.needs_cleanup_role,
            "RESET SESSION AUTHORIZATION must leave role cleanup armed while \
             the surrounding transaction can roll it back"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reset_all_invalidates_startup_only_server_parameter_mirror() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server
            .server_parameters
            .set_param("search_path", "tenant_a", true);
        server
            .server_parameters
            .set_param("client.app_user", "alice", true);
        server
            .server_parameters
            .set_param("application_name", "svc-a", true);
        server.track_reset_cleanup_commands([ResetCleanupCommand::ResetAll]);

        handle_command_complete(&mut server, &BytesMut::from(&b"RESET\0"[..]));
        handle_ready_for_query(&mut server, &mut BytesMut::from(&b"I"[..]))
            .expect("valid idle ReadyForQuery");

        let params = server.server_parameters.as_hashmap();
        assert!(
            !params.contains_key("search_path"),
            "RESET ALL must invalidate startup-only planner GUC mirror"
        );
        assert!(
            !params.contains_key("client.app_user"),
            "RESET ALL must invalidate startup-only custom GUC mirror"
        );
        assert_eq!(
            params.get("application_name").map(String::as_str),
            Some("svc-a"),
            "ParameterStatus-tracked GUC mirror should be preserved"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discard_all_invalidates_startup_only_server_parameter_mirror() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server
            .server_parameters
            .set_param("search_path", "tenant_a", true);
        server
            .server_parameters
            .set_param("client.app_user", "alice", true);
        server
            .server_parameters
            .set_param("application_name", "svc-a", true);

        handle_command_complete(&mut server, &BytesMut::from(&b"DISCARD ALL\0"[..]));

        let params = server.server_parameters.as_hashmap();
        assert!(
            !params.contains_key("search_path"),
            "DISCARD ALL must invalidate startup-only planner GUC mirror"
        );
        assert!(
            !params.contains_key("client.app_user"),
            "DISCARD ALL must invalidate startup-only custom GUC mirror"
        );
        assert_eq!(
            params.get("application_name").map(String::as_str),
            Some("svc-a"),
            "ParameterStatus-tracked GUC mirror should be preserved"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn set_local_default_does_not_disarm_identity_cleanup() {
        use crate::server::Server;

        let mut server = Server::test_dead_socket();
        server.cleanup_state.needs_cleanup_role = true;
        server.track_set_cleanup_commands(extract_set_cleanup_commands(b"SET LOCAL ROLE DEFAULT"));

        handle_command_complete(&mut server, &BytesMut::from(&b"SET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_role,
            "SET LOCAL ROLE DEFAULT is transaction-local and must not disarm session role cleanup"
        );

        let mut server = Server::test_dead_socket();
        server.cleanup_state.needs_cleanup_role = true;
        server.cleanup_state.needs_cleanup_session_authorization = true;
        server.track_set_cleanup_commands(extract_set_cleanup_commands(
            b"SET LOCAL SESSION AUTHORIZATION DEFAULT",
        ));

        handle_command_complete(&mut server, &BytesMut::from(&b"SET\0"[..]));

        assert!(
            server.cleanup_state.needs_cleanup_session_authorization,
            "SET LOCAL SESSION AUTHORIZATION DEFAULT must not disarm session auth cleanup"
        );
        assert!(
            server.cleanup_state.needs_cleanup_role,
            "SET LOCAL SESSION AUTHORIZATION DEFAULT must not disarm role cleanup"
        );
    }

    #[test]
    fn declare_cursor_tag_arms_declare_cleanup() {
        assert_eq!(
            classify_command_complete(b"DECLARE CURSOR\0"),
            CommandCompleteEffect::ArmDeclare,
        );
    }

    #[test]
    fn close_cursor_all_tag_disarms_declare_cleanup() {
        assert_eq!(
            classify_command_complete(b"CLOSE CURSOR ALL\0"),
            CommandCompleteEffect::DisarmDeclare,
        );
    }

    #[test]
    fn close_single_cursor_tag_is_inert() {
        // Closing one named cursor is not the same as `CLOSE ALL` — other
        // cursors may still be open, so this tag must NOT disarm declare-cleanup.
        assert_eq!(
            classify_command_complete(b"CLOSE CURSOR\0"),
            CommandCompleteEffect::None,
        );
    }

    #[test]
    fn deallocate_all_tag_disarms_prepare_cleanup() {
        assert_eq!(
            classify_command_complete(b"DEALLOCATE ALL\0"),
            CommandCompleteEffect::DisarmPrepare,
        );
    }

    #[test]
    fn prepare_tag_is_not_inert() {
        assert_eq!(
            classify_command_complete(b"PREPARE\0"),
            CommandCompleteEffect::ArmPrepare,
            "SQL-level PREPARE must arm prepared-statement cleanup"
        );
    }

    #[test]
    fn prepare_effect_arms_cleanup_state() {
        let src = include_str!("protocol_io.rs");
        let handler_start = src
            .find("fn handle_command_complete(")
            .expect("CommandComplete handler not found");
        let handler_body = &src[handler_start..];
        let handler_end = handler_body
            .find("\n}\n\n/// Handles ParameterStatus")
            .expect("CommandComplete handler end not found");
        let handler_body = &handler_body[..handler_end];

        assert!(
            handler_body.contains("CommandCompleteEffect::ArmPrepare")
                && handler_body.contains("server.cleanup_state.needs_cleanup_prepare = true"),
            "successful SQL-level PREPARE must arm DEALLOCATE ALL cleanup on checkin"
        );
    }

    #[test]
    fn discard_all_tag_disarms_every_cleanup_flag() {
        assert_eq!(
            classify_command_complete(b"DISCARD ALL\0"),
            CommandCompleteEffect::DisarmAll,
        );
    }

    #[test]
    fn partial_discard_tags_are_inert() {
        // DISCARD PLANS drops the plan cache, DISCARD TEMP drops temp tables,
        // DISCARD SEQUENCES resets sequence caches. None of them revert SET
        // state or drop prepared statements, so none should influence the
        // cleanup flags on their own.
        assert_eq!(
            classify_command_complete(b"DISCARD PLANS\0"),
            CommandCompleteEffect::None,
        );
        assert_eq!(
            classify_command_complete(b"DISCARD TEMP\0"),
            CommandCompleteEffect::None,
        );
        assert_eq!(
            classify_command_complete(b"DISCARD SEQUENCES\0"),
            CommandCompleteEffect::None,
        );
    }

    #[test]
    fn regular_command_tags_are_inert() {
        // A representative sample of data-plane tags. If any of these ever
        // start influencing cleanup tracking it will be a correctness bug.
        for tag in [
            &b"SELECT 1\0"[..],
            b"INSERT 0 1\0",
            b"UPDATE 5\0",
            b"DELETE 10\0",
            b"BEGIN\0",
            b"COMMIT\0",
            b"ROLLBACK\0",
            b"UNLISTEN\0",
            b"SAVEPOINT\0",
        ] {
            assert_eq!(
                classify_command_complete(tag),
                CommandCompleteEffect::None,
                "tag {:?} should not influence cleanup",
                std::str::from_utf8(tag).unwrap_or("<non-utf8>"),
            );
        }
    }

    #[test]
    fn length_only_matches_do_not_confuse_classifier() {
        // Both DECLARE CURSOR and DEALLOCATE ALL are 15 bytes long with the
        // trailing NUL; the classifier must dispatch on content, not length.
        assert_eq!(
            classify_command_complete(b"DECLARE CURSOR\0"),
            CommandCompleteEffect::ArmDeclare,
        );
        assert_eq!(
            classify_command_complete(b"DEALLOCATE ALL\0"),
            CommandCompleteEffect::DisarmPrepare,
        );
        // Same length as DEALLOCATE ALL but unrelated content — must be inert.
        assert_eq!(
            classify_command_complete(b"MADE UP TAG 01\0"),
            CommandCompleteEffect::None,
        );
    }

    #[test]
    fn empty_or_missing_nul_is_inert() {
        assert_eq!(classify_command_complete(b""), CommandCompleteEffect::None,);
        // Without the trailing NUL the length never matches the expected one.
        assert_eq!(
            classify_command_complete(b"SET"),
            CommandCompleteEffect::None,
        );
        assert_eq!(
            classify_command_complete(b"RESET"),
            CommandCompleteEffect::None,
        );
    }

    #[test]
    fn large_message_header_flushes_are_deadline_bound() {
        let src = include_str!("protocol_io.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("test module should follow implementation");
            &src[..tests_start]
        };

        assert!(
            impl_src.contains(
                "write_all_flush_timeout_counted(\n        client_stream,\n        &server.buffer,"
            ),
            "large-message header flushes to the client must be bounded by proxy_copy_data_timeout"
        );
        assert!(
            !impl_src.contains("write_all_flush(client_stream, &server.buffer)"),
            "large-message handlers must not flush headers with an unbounded client write"
        );
    }

    /// Client write half that takes `limit` bytes, then fails like a
    /// connection the client reset mid-frame.
    struct FailAfter {
        limit: usize,
        taken: usize,
    }

    impl tokio::io::AsyncWrite for FailAfter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.taken >= self.limit {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            let n = buf.len().min(self.limit - self.taken);
            self.taken += n;
            std::task::Poll::Ready(Ok(n))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A client gone in the middle of a streamed frame leaves the backend in
    /// step with the protocol: the rest of the frame is read, the exchange
    /// reports the client, and the rest of the reply can still be drained,
    /// so the query is stopped with its pool slot held.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_client_gone_mid_frame_leaves_the_backend_in_step() {
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.max_message_size = 1024;
        let value = vec![b'x'; 200 * 1024];
        let mut reply = BytesMut::new();
        reply.put_u8(b'D');
        reply.put_i32(4 + 2 + 4 + value.len() as i32);
        reply.put_i16(1);
        reply.put_i32(value.len() as i32);
        reply.put_slice(&value);
        let mut rest = crate::messages::command_complete("SELECT 1");
        rest.put(crate::messages::ready_for_query(false));
        reply.put_slice(&rest);
        let writer = tokio::spawn(async move {
            peer.write_all(&reply).await.unwrap();
            peer
        });

        let mut client = FailAfter {
            limit: 10_000,
            taken: 0,
        };
        let result = server.recv(&mut client, None).await;

        assert!(
            matches!(result, Err(Error::ClientGoneMidStream(_))),
            "{result:?}"
        );
        assert!(!server.is_bad(), "the backend is still in step");
        assert!(server.is_data_available());
        let drained = server.recv(tokio::io::sink(), None).await.unwrap();
        assert_eq!(&drained[..], &rest[..]);
        let _peer = writer.await.unwrap();
    }

    /// A large DataRow behind a RowDescription is streamed by the recv after
    /// the one that returned the RowDescription. A client gone in the middle
    /// of it still has the frame read whole, so the next recv starts at the
    /// message after it instead of taking that for the rest of the row.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_client_gone_mid_deferred_frame_leaves_the_backend_in_step() {
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.max_message_size = 1024;
        let value = vec![b'x'; 200 * 1024];
        let mut reply = BytesMut::new();
        reply.put_u8(b'T');
        reply.put_i32(4 + 2 + 2 + 18);
        reply.put_i16(1);
        reply.put_slice(b"v\0");
        reply.put_slice(&[0; 18]);
        reply.put_u8(b'D');
        reply.put_i32(4 + 2 + 4 + value.len() as i32);
        reply.put_i16(1);
        reply.put_i32(value.len() as i32);
        reply.put_slice(&value);
        let mut rest = crate::messages::command_complete("SELECT 1");
        rest.put(crate::messages::ready_for_query(false));
        reply.put_slice(&rest);
        let writer = tokio::spawn(async move {
            peer.write_all(&reply).await.unwrap();
            peer
        });

        let mut client = FailAfter {
            limit: 10_000,
            taken: 0,
        };
        let described = server.recv(&mut client, None).await.unwrap();
        assert_eq!(described.first(), Some(&b'T'));
        assert!(server.pending_large_message.is_some());
        let result = server.recv(&mut client, None).await;
        assert!(
            matches!(result, Err(Error::ClientGoneMidStream(_))),
            "{result:?}"
        );
        assert!(
            server.pending_large_message.is_none(),
            "the frame was read whole"
        );
        assert!(!server.is_bad());
        let drained = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            server.recv(tokio::io::sink(), None),
        )
        .await
        .expect("the rest of the reply is read")
        .unwrap();
        assert_eq!(&drained[..], &rest[..]);
        let _peer = writer.await.unwrap();
    }

    /// The bytes of a streamed frame a client took before it failed count as
    /// streamed, those of the chunk it failed in too.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn frame_bytes_a_client_took_before_it_failed_are_counted() {
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.address.username = "streaming_partial_user".to_string();
        server.address.database = "streaming_partial_db".to_string();
        server.max_message_size = 1024;
        let value = vec![b'x'; 200 * 1024];
        let mut reply = BytesMut::new();
        reply.put_u8(b'D');
        reply.put_i32(4 + 2 + 4 + value.len() as i32);
        reply.put_i16(1);
        reply.put_i32(value.len() as i32);
        reply.put_slice(&value);
        reply.put(crate::messages::command_complete("SELECT 1"));
        reply.put(crate::messages::ready_for_query(false));
        let writer = tokio::spawn(async move {
            peer.write_all(&reply).await.unwrap();
            peer
        });
        let counter = crate::web::metrics::STREAMING_BYTES_TOTAL.with_label_values(&[
            "streaming_partial_user",
            "streaming_partial_db",
            "data_row",
        ]);
        let before = counter.get();

        let mut client = FailAfter {
            limit: 10_000,
            taken: 0,
        };
        let result = server.recv(&mut client, None).await;

        assert!(matches!(result, Err(Error::ClientGoneMidStream(_))));
        assert_eq!(counter.get() - before, 10_000);
        let _peer = writer.await.unwrap();
    }

    /// Once its client has gone, the rest of a streamed frame is read within
    /// the time an abandoned query gets, however steadily the backend keeps
    /// sending it; a backend slower than that is closed. Being in the middle
    /// of writing the frame, PostgreSQL notices that at once.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn the_rest_of_a_frame_its_client_left_is_read_in_bounded_time() {
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.max_message_size = 1024;
        server.abandoned_query_timeouts = crate::server::AbandonedQueryTimeouts {
            finish: std::time::Duration::from_millis(20),
            after_cancel: std::time::Duration::from_millis(200),
        };
        let value_len: usize = 20 * 1024;
        let mut head = BytesMut::new();
        head.put_u8(b'D');
        head.put_i32(4 + 2 + 4 + value_len as i32);
        head.put_i16(1);
        head.put_i32(value_len as i32);
        head.put_slice(&vec![b'x'; 16 * 1024]);
        let trickle = tokio::spawn(async move {
            peer.write_all(&head).await.unwrap();
            // The rest comes a byte a second: slow, but never stalled.
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if peer.write_all(b"x").await.is_err() {
                    break;
                }
            }
        });

        let mut client = FailAfter {
            limit: 10_000,
            taken: 0,
        };
        let started = tokio::time::Instant::now();
        let result = server.recv(&mut client, None).await;

        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "read for {:?}",
            started.elapsed()
        );
        assert!(result.is_err());
        assert!(server.is_bad());
        trickle.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn large_copy_data_bounds_stalled_backend_with_timeout() {
        // a COPY-OUT ('d') frame whose
        // backend stalls mid-stream on a live-but-silent socket must be
        // bounded by `proxy_copy_data_timeout`, not hang forever. We declare
        // a 1 MiB payload that the silent peer never sends and pass a short
        // copy timeout; the handler must return Err and mark the backend bad.
        // The outer 5 s guard turns a regression (missing internal timeout)
        // into a test failure instead of a hung suite.
        use super::{handle_large_copy_data_inner, Server};
        use std::time::Duration;

        // Peer stays alive but silent, so reads on the backend stream block
        // (a closed peer would EOF immediately and never exercise the timeout).
        let (mut server, _peer) = Server::test_silent_socket();
        let mut client = tokio::io::sink();
        let declared_len: i32 = 4 + 1_000_000; // 4-byte self-length + 1 MiB body

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            handle_large_copy_data_inner(
                &mut server,
                &mut client,
                b'd',
                declared_len,
                Duration::from_millis(80),
            ),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "handle_large_copy_data must return within its own copy timeout, not hang"
        );
        let res = outcome.expect("handler did not hang");
        assert!(res.is_err(), "stalled COPY-OUT backend must yield an error");
        assert!(
            server.is_bad(),
            "backend must be marked bad after a COPY-OUT timeout so it is evicted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_copy_out_flush_keeps_reading_after_copy_response() {
        // In Flush-only async mode, Execute is counted as one
        // expected response. A COPY OUT Execute starts with CopyOutResponse
        // but is not complete until CommandComplete. If CopyOutResponse
        // consumes the expected response slot, the next recv() exits at the
        // top-level async guard and leaves CopyData unread on the backend
        // socket.
        use super::Server;
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.set_async_mode(true);
        server.set_expected_responses(1);

        peer.write_all(&[
            b'H', 0, 0, 0, 8, 0, 0, 0, 0, // CopyOutResponse: overall text, 0 columns
            b'd', 0, 0, 0, 8, b'd', b'a', b't', b'a', // CopyData("data")
            b'c', 0, 0, 0, 4, // CopyDone
            b'C', 0, 0, 0, 11, b'C', b'O', b'P', b'Y', b' ', b'1', 0, // CommandComplete
        ])
        .await
        .expect("peer must write COPY OUT response sequence");

        let first = server
            .recv(tokio::io::sink(), None)
            .await
            .expect("CopyOutResponse must be relayed");
        assert_eq!(first.first(), Some(&b'H'));
        assert!(server.in_copy_mode(), "COPY OUT must enter copy mode");
        assert_eq!(
            server.expected_responses(),
            1,
            "CopyOutResponse is not the terminal Execute response"
        );

        let rest = server
            .recv(tokio::io::sink(), None)
            .await
            .expect("CopyData through CommandComplete must be drained");
        assert_eq!(rest.first(), Some(&b'd'));
        assert!(
            rest.windows(5).any(|w| w == [b'c', 0, 0, 0, 4]),
            "CopyDone must be included in the drained response"
        );
        assert!(
            rest.windows(5).any(|w| w == [b'C', 0, 0, 0, 11]),
            "CommandComplete must be included in the drained response"
        );
        assert_eq!(server.expected_responses(), 0);
        assert!(!server.in_copy_mode(), "CommandComplete exits copy mode");
        assert!(
            !server.is_data_available(),
            "COPY OUT stream was fully drained"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_execute_select_flush_keeps_reading_after_row_description() {
        // A RowDescription can be the first response to an Execute SELECT, but
        // the Execute is not complete until CommandComplete. The async Flush
        // counter must not stop the recv loop before DataRow/CommandComplete.
        use super::Server;
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.set_async_mode(true);
        server.set_expected_responses(1);

        peer.write_all(&[
            b'T', 0, 0, 0, 4, // RowDescription with no columns; enough for relay accounting
            b'D', 0, 0, 0, 4, // DataRow with no body
            b'C', 0, 0, 0, 13, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1', 0,
        ])
        .await
        .expect("peer must write SELECT response sequence");

        let response = server
            .recv(tokio::io::sink(), None)
            .await
            .expect("SELECT response must be relayed");

        assert_eq!(response.first(), Some(&b'T'));
        assert!(
            response.windows(5).any(|w| w == [b'D', 0, 0, 0, 4]),
            "DataRow must be included in the drained response"
        );
        assert!(
            response.windows(5).any(|w| w == [b'C', 0, 0, 0, 13]),
            "CommandComplete must be included in the drained response"
        );
        assert_eq!(server.expected_responses(), 0);
        assert!(
            !server.is_data_available(),
            "SELECT response stream was fully drained"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_execute_select_before_describe_keeps_describe_response() {
        // A later Describe must not make the Execute SELECT RowDescription
        // consume its terminal slot. Otherwise Flush exits after the Execute
        // CommandComplete and leaves the Describe response unread.
        use super::Server;
        use crate::server::AsyncExpectedResponse;
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.set_async_mode(true);
        server.set_expected_response_sequence([
            AsyncExpectedResponse::Operation,
            AsyncExpectedResponse::Describe,
        ]);

        peer.write_all(&[
            b'T', 0, 0, 0, 4, // Execute RowDescription
            b'D', 0, 0, 0, 4, // Execute DataRow
            b'C', 0, 0, 0, 13, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1', 0, b'T', 0, 0, 0,
            4, // Describe Portal RowDescription
        ])
        .await
        .expect("peer must write Execute then Describe responses");

        let response = server
            .recv(tokio::io::sink(), None)
            .await
            .expect("Execute and Describe responses must be relayed");

        assert!(
            response
                .windows(5)
                .filter(|w| *w == [b'T', 0, 0, 0, 4])
                .count()
                == 2,
            "both RowDescription frames must be included"
        );
        assert_eq!(server.expected_responses(), 0);
        assert!(
            !server.is_data_available(),
            "Describe response must not be left unread"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bulk_relay_hands_back_a_buffer_that_survives_the_next_cycle() {
        // The flush check runs AFTER the message is appended, so a bulk
        // response always leaves the buffer past `response_flush_threshold`
        // and the hand-off takes the `mem::replace` arm. If the replacement is
        // sized at exactly the threshold it is guaranteed to grow again on the
        // very next cycle, so every flush of a large report pays one realloc
        // plus a full memcpy of the accumulated bytes.
        use super::Server;
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.set_async_mode(true);
        server.set_expected_responses(1);
        // Lower the configured threshold instead of writing 64 KiB+ of rows:
        // the invariant under test is relative to the threshold, not to any
        // particular byte count, and a small value keeps the test cheap.
        server.response_flush_threshold = 8192;
        let flush_threshold = server.response_flush_threshold;

        // DataRows summing past the threshold, so the relay breaks on the
        // buffer limit rather than on ReadyForQuery.
        let row_count = flush_threshold.div_ceil(1024) + 4;
        let peer_task = tokio::spawn(async move {
            let row_payload = vec![b'x'; 1024];
            let mut wire = Vec::new();
            for _ in 0..row_count {
                let len = (row_payload.len() + 4) as i32;
                wire.push(b'D');
                wire.extend_from_slice(&len.to_be_bytes());
                wire.extend_from_slice(&row_payload);
            }
            // Writing from a task keeps the socket draining while `recv` reads;
            // a direct write of this size would block on the socket buffer.
            let _ = peer.write_all(&wire).await;
        });

        let relayed = server
            .recv(tokio::io::sink(), None)
            .await
            .expect("bulk DataRows must be relayed");
        peer_task.abort();
        assert!(
            relayed.len() > flush_threshold,
            "this test must exercise the oversized hand-off arm"
        );
        assert!(
            server.buffer.capacity() > flush_threshold,
            "the replacement buffer must have room past the flush threshold, \
             otherwise the next cycle reallocates and memcpies again (capacity {})",
            server.buffer.capacity()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relay_batches_up_to_the_configured_flush_threshold() {
        // `general.response_flush_threshold` must actually drive the relay:
        // raising it has to make one recv() carry more of a bulk response,
        // which is the whole point of making the constant configurable.
        use super::Server;
        use tokio::io::AsyncWriteExt;

        async fn relayed_len_for_threshold(threshold: usize) -> usize {
            let (mut server, mut peer) = Server::test_silent_socket();
            server.set_async_mode(true);
            server.set_expected_responses(1);
            server.response_flush_threshold = threshold;

            // Enough rows to overshoot the largest threshold under test.
            let peer_task = tokio::spawn(async move {
                let row_payload = vec![b'x'; 1024];
                let mut wire = Vec::new();
                for _ in 0..80 {
                    let len = (row_payload.len() + 4) as i32;
                    wire.push(b'D');
                    wire.extend_from_slice(&len.to_be_bytes());
                    wire.extend_from_slice(&row_payload);
                }
                let _ = peer.write_all(&wire).await;
            });

            let relayed = server
                .recv(tokio::io::sink(), None)
                .await
                .expect("bulk DataRows must be relayed");
            peer_task.abort();
            relayed.len()
        }

        let small = relayed_len_for_threshold(8 * 1024).await;
        let large = relayed_len_for_threshold(64 * 1024).await;

        assert!(
            (8 * 1024..64 * 1024).contains(&small),
            "an 8 KiB threshold must break the relay right past 8 KiB (got {small})"
        );
        assert!(
            large >= 64 * 1024,
            "a 64 KiB threshold must batch the response up to 64 KiB (got {large})"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_both_response_fails_closed_without_relay() {
        // COPY BOTH requires a full-duplex replication pump. The normal
        // transaction relay only alternates client/server reads, so forwarding
        // CopyBothResponse would expose a protocol mode pg_doorman cannot keep
        // synchronized.
        use super::Server;
        use crate::errors::Error;
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.set_async_mode(true);
        server.set_expected_responses(1);

        peer.write_all(&[
            b'W', 0, 0, 0, 7, 0, 0, 0, // CopyBothResponse: overall text, 0 columns
        ])
        .await
        .expect("peer must write CopyBothResponse");

        let err = server
            .recv(tokio::io::sink(), None)
            .await
            .expect_err("CopyBothResponse must fail closed");

        assert!(
            matches!(err, Error::ProtocolSyncError(_)),
            "unexpected error: {err:?}"
        );
        assert!(server.is_bad(), "backend must be evicted after COPY BOTH");
        assert!(
            !server.in_copy_mode(),
            "unsupported COPY BOTH must not leave copy mode armed"
        );
        assert!(
            !server.is_data_available(),
            "unsupported COPY BOTH must not advertise buffered backend data"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn large_non_streamable_frame_is_relayed_without_evicting_backend() {
        // A non-DataRow backend frame larger than `message_size_to_be_stream`
        // (1 KiB here) must be relayed to the client like any other frame and
        // must NOT mark the backend bad. `message_size_to_be_stream` decides
        // only whether a large DataRow/CopyData is streamed; it is not a hard
        // reject ceiling for other frame types. A large PL/pgSQL
        // NoticeResponse/ErrorResponse must pass through up to MAX_MESSAGE_SIZE
        // (256 MiB), the only ceiling that rejects a non-streamable frame.
        use super::Server;
        use tokio::io::AsyncWriteExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.max_message_size = 1024;

        // NoticeResponse 'N', message_len = 2000 (> 1 KiB cap, < 256 MiB):
        // 1 type byte + 4-byte length field (value 2000) + 1996 body bytes.
        let notice_len: i32 = 2000;
        let mut notice = vec![b'N'];
        notice.extend_from_slice(&notice_len.to_be_bytes());
        notice.extend_from_slice(&vec![0u8; notice_len as usize - 4]);
        peer.write_all(&notice)
            .await
            .expect("peer must write the large NoticeResponse");
        // ReadyForQuery('I') terminates the relay loop.
        peer.write_all(&[b'Z', 0, 0, 0, 5, b'I'])
            .await
            .expect("peer must write ReadyForQuery");

        let response = server
            .recv(tokio::io::sink(), None)
            .await
            .expect("large non-streamable NoticeResponse must be relayed, not rejected");

        assert_eq!(
            response.first(),
            Some(&b'N'),
            "the NoticeResponse must be forwarded to the client"
        );
        assert!(
            !server.is_bad(),
            "an oversize non-streamable frame must not evict a healthy backend"
        );
    }
}
