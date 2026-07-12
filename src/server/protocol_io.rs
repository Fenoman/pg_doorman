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
fn sanitize_for_log(s: &str) -> String {
    if s.contains(['\n', '\r']) {
        s.replace('\n', "\\n").replace('\r', "\\r")
    } else {
        s.to_string()
    }
}

use tokio::time::timeout;

use crate::config::config_arc;
use crate::errors::Error;
use crate::errors::Error::MaxMessageSize;
use crate::messages::PgErrorMsg;
use crate::messages::MAX_MESSAGE_SIZE;
use crate::messages::{
    proxy_copy_data_with_timeout, read_message_body_reuse, read_message_header, write_all_flush,
    write_all_flush_timeout, BytesMutReader,
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
/// `DISCARD ALL` CommandComplete tag — equivalent to `RESET ALL; DEALLOCATE ALL;
/// CLOSE ALL; UNLISTEN *; ...`, so disarms every `needs_cleanup_*` flag.
const COMMAND_COMPLETE_BY_DISCARD_ALL: &[u8; 12] = b"DISCARD ALL\0";

/// Buffer flush threshold in bytes (8 KiB).
/// When the buffer reaches this size, it will be flushed to avoid excessive memory usage.
const BUFFER_FLUSH_THRESHOLD: usize = 8192;

/// Flushes messages within `duration`; timeout marks the server bad.
pub(crate) async fn send_and_flush_timeout(
    server: &mut Server,
    messages: &BytesMut,
    duration: Duration,
) -> Result<(), Error> {
    match timeout(duration, send_and_flush(server, messages)).await {
        Ok(result) => result,
        Err(err) => {
            server.mark_bad("flush timeout");
            error!(
                "[{}@{}] flush timeout pid={}: {err}",
                server.address.username,
                server.address.pool_name,
                server.get_process_id(),
            );
            Err(Error::FlushTimeout)
        }
    }
}

/// Flushes messages and records write stats/activity.
pub(crate) async fn send_and_flush(server: &mut Server, messages: &BytesMut) -> Result<(), Error> {
    server.stats.data_sent(messages.len());
    server.stats.wait_writing();

    match write_all_flush(&mut *server.stream, messages).await {
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
    let copy_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
    // Send current buffer + header
    server.buffer.put_u8(code_u8);
    server.buffer.put_i32(message_len);
    let prev_bad = server.bad;
    server.bad = true;
    write_all_flush_timeout(client_stream, &server.buffer, copy_timeout).await?;

    // Header (1 byte type code + 4 byte length field) already left
    // pg_doorman in the deadline-bound flush above; the payload is what
    // `proxy_copy_data_with_timeout` streams. The counter is bumped
    // by header + actually-forwarded payload so a partial copy is
    // recorded as the bytes that actually reached the wire, not the
    // declared frame size that promised more than was delivered.
    const HEADER_BYTES: u64 = 1 + mem::size_of::<i32>() as u64;
    let mut payload_copied: usize = 0;
    let res = proxy_copy_data_with_timeout(
        copy_timeout,
        &mut *server.stream,
        client_stream,
        message_len as usize - mem::size_of::<i32>(),
        &mut payload_copied,
    )
    .await;
    record_streaming(
        server,
        "data_row",
        res.is_ok(),
        HEADER_BYTES + payload_copied as u64,
    );
    if let Err(err) = res {
        server.mark_bad(err.to_string().as_str());
        return Err(err);
    }

    if !prev_bad {
        server.bad = false;
    }

    server
        .stats
        .data_received(server.buffer.len() + message_len as usize);
    server.touch_activity();
    server.data_available = true;
    server.stats.wait_idle();
    // zero-copy split - was `buffer.clear(); buffer.clone()` which
    // returned an empty BytesMut after a full deep copy of the (now-empty)
    // buffer. Just hand back a fresh empty BytesMut; semantics identical.
    server.buffer.clear();
    Ok(BytesMut::new())
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
    server.buffer.put_u8(code_u8);
    server.buffer.put_i32(message_len);
    let prev_bad = server.bad;
    server.bad = true;
    write_all_flush(client_stream, &server.buffer).await?;

    const HEADER_BYTES: u64 = 1 + mem::size_of::<i32>() as u64;
    let mut payload_copied: usize = 0;
    let res = proxy_copy_data_with_timeout(
        get_config().general.proxy_copy_data_timeout.as_std(),
        &mut server.stream,
        client_stream,
        message_len as usize - mem::size_of::<i32>(),
        &mut payload_copied,
    )
    .await;
    record_streaming(
        server,
        "function_call_response",
        res.is_ok(),
        HEADER_BYTES + payload_copied as u64,
    );
    if let Err(err) = res {
        server.mark_bad(err.to_string().as_str());
        return Err(err);
    }

    if !prev_bad {
        server.bad = false;
    }

    server
        .stats
        .data_received(server.buffer.len() + message_len as usize);
    server.last_activity = SystemTime::now();
    server.data_available = true;
    server.buffer.clear();
    server.stats.wait_idle();
    Ok(server.buffer.clone())
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
    let copy_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
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
    // Send current buffer + header
    server.buffer.put_u8(code_u8);
    server.buffer.put_i32(message_len);
    let prev_bad = server.bad;
    server.bad = true;
    write_all_flush_timeout(client_stream, &server.buffer, copy_timeout).await?;

    // Same wire-bytes contract as in `handle_large_data_row`: header
    // is on the wire after the buffer flush above, the payload is
    // counted from what `proxy_copy_data_with_timeout` actually shipped.
    const HEADER_BYTES: u64 = 1 + mem::size_of::<i32>() as u64;
    let mut payload_copied: usize = 0;
    // bound the COPY-OUT stream with
    // `proxy_copy_data_timeout`, exactly like the sibling
    // `handle_large_data_row`. Without the deadline a backend that stalls
    // mid-frame on a live-but-silent socket pins this task and the
    // checked-out backend forever (TCP keepalive cannot break an
    // app-level stall; TCP_USER_TIMEOUT is Linux-only and fires only on
    // un-ACKed in-flight data). On timeout the backend is marked bad so it
    // is evicted instead of recycled with undrained bytes.
    let res = proxy_copy_data_with_timeout(
        copy_timeout,
        &mut *server.stream,
        client_stream,
        message_len as usize - mem::size_of::<i32>(),
        &mut payload_copied,
    )
    .await;
    record_streaming(
        server,
        "copy_data",
        res.is_ok(),
        HEADER_BYTES + payload_copied as u64,
    );
    if let Err(err) = res {
        server.mark_bad(err.to_string().as_str());
        return Err(err);
    }

    server.bad = prev_bad;
    server
        .stats
        .data_received(server.buffer.len() + message_len as usize);
    server.touch_activity();
    server.stats.wait_idle();
    // clone()-after-clear was copying an empty buffer. Hand back a fresh
    // BytesMut directly.
    server.buffer.clear();
    Ok(BytesMut::new())
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

/// Handles ErrorResponse ('E') message from the server.
/// Logs the error and updates server state accordingly.
fn handle_error_response(server: &mut Server, message: &mut BytesMut) {
    server.response_cycle_had_error = true;
    if let Ok(msg) = PgErrorMsg::parse(message) {
        let mut details = format!(
            "[{}@{}] server error pid={}: severity={}, code={}, message=\"{}\", in_transaction={}, in_copy={}",
            server.address.username, server.address.pool_name, server.get_process_id(),
            msg.severity, msg.code, sanitize_for_log(&msg.message),
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
        // Let `small_simple_query` return SQL-level failures as `Err`.
        server.last_sql_error = Some((msg.code.clone(), msg.message.clone()));
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

    // Reset prepared statements cache on error
    if server.prepared_statement_cache.is_some() {
        server.cleanup_state.needs_cleanup_prepare = true;
    }

    // A Parse error means PostgreSQL did not install any pending prepared
    // statement names. Drop the optimistic LRU entries so the next Bind
    // re-Parses instead of hitting a stale DOORMAN_N.
    if !server.registering_prepared_statement.is_empty() {
        let pending: Vec<String> = server.registering_prepared_statement.drain(..).collect();
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
        // was `needs_cleanup()` - a getter whose bool
        // result was silently dropped. In session-mode (where
        // mark_bad is NOT called below), the backend used to return
        // to the SAME client with cleanup_state untouched, so any
        // SET that the failed batch performed before the error
        // leaked across the next checkout. set_true() marks all
        // three cleanup buckets (SET / PREPARE / DECLARE), forcing
        // RESET ALL + DEALLOCATE ALL on the next checkin.
        server.cleanup_state.set_true();
        if !server.session_mode {
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
    } else if tag == COMMAND_COMPLETE_BY_DISCARD_ALL {
        CommandCompleteEffect::DisarmAll
    } else {
        CommandCompleteEffect::None
    }
}

/// Drop the pg_doorman-side prepared statement LRU after the server confirms it
/// just executed an equivalent of `DEALLOCATE ALL` or `DISCARD ALL`.
fn drop_prepared_statement_cache_on_reset(server: &mut Server, reason: &'static str) {
    server.registering_prepared_statement.clear();
    let Some(cache_size) = server
        .prepared_statement_cache
        .as_ref()
        .map(|cache| cache.len())
    else {
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
    }
}

fn defer_set_cleanup_disarm_if_transactionally_safe(server: &mut Server) {
    if server.in_transaction() || server.command_complete_in_transaction {
        return;
    }
    server.pending_cleanup_disarms.set = true;
    server.pending_cleanup_disarms.startup_parameter_mirror = true;
}

fn defer_role_cleanup_disarm_if_transactionally_safe(server: &mut Server) {
    if server.in_transaction() || server.command_complete_in_transaction {
        return;
    }
    server.pending_cleanup_disarms.role = true;
}

fn defer_session_authorization_cleanup_disarm_if_transactionally_safe(server: &mut Server) {
    if server.in_transaction() || server.command_complete_in_transaction {
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
        }
        CommandCompleteEffect::DisarmSet => {
            defer_set_cleanup_disarm_if_transactionally_safe(server);
        }
        CommandCompleteEffect::DisarmDeclare => {
            server.cleanup_state.needs_cleanup_declare = false;
        }
        CommandCompleteEffect::DisarmPrepare => {
            server.cleanup_state.needs_cleanup_prepare = false;
            drop_prepared_statement_cache_on_reset(server, "DEALLOCATE ALL");
        }
        CommandCompleteEffect::DisarmAll => {
            server.cleanup_state.reset();
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

/// Receive data from the server in response to a client request.
/// Must be called multiple times while `server.is_data_available()` is true.
pub(crate) async fn recv<C>(
    server: &mut Server,
    mut client_stream: C,
    mut client_server_parameters: Option<&mut ServerParameters>,
) -> Result<BytesMut, Error>
where
    C: tokio::io::AsyncWrite + std::marker::Unpin,
{
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
        if result.is_ok() {
            // Clear deferred header only after successful handling.
            // On error we must keep it, otherwise the next recv() call
            // starts from the middle of a large frame and breaks protocol sync.
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
    loop {
        // In async mode, check if all expected responses have been received
        if server.is_async() && server.expected_responses() == 0 {
            server.data_available = false;
            break;
        }

        let (code_u8, message_len) = read_message_header(&mut *server.stream).await?;
        // Handle large DataRow messages that exceed max_message_size
        if server.max_message_size > 0
            && message_len > server.max_message_size
            && code_u8 as char == 'D'
        {
            // If buffer has accumulated messages (e.g. BindComplete, RowDescription),
            // return them first so execute_server_roundtrip can run
            // reorder_parse_complete_responses before we stream to client.
            if !server.buffer.is_empty() {
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
            if !server.buffer.is_empty() {
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
            if !server.buffer.is_empty() {
                server.pending_large_message = Some((code_u8, message_len));
                server.data_available = true;
                let result = server.buffer.clone();
                server.buffer.clear();
                server.stats.data_received(result.len());
                server.last_activity = SystemTime::now();
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

        // Read body into per-connection reusable buffer (header already consumed above).
        // per-iter `stats.wait_idle()` dropped - the
        // `WaitIdleOnDrop` guard above restores idle once the recv
        // loop returns to the caller. Observers only ever sampled
        // the nibble at admin-poll cadence; the transient
        // "idle-between-reads" flicker was pure observability noise.
        let mut message = match read_message_body_reuse(
            &mut *server.stream,
            &mut server.read_buf,
            code_u8,
            message_len,
        )
        .await
        {
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

            // DataRow
            'D' => {
                // More data is available after this message, this is not the end of the reply.
                server.data_available = true;

                // Don't flush yet, the more we buffer, the faster this goes...up to a limit.
                if server.buffer.len() >= BUFFER_FLUSH_THRESHOLD {
                    break;
                }
            }

            // CopyInResponse: copy is starting from client to server.
            'G' => {
                server.in_copy_mode = true;
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

            // CopyData
            'd' => {
                // Don't flush yet, buffer until we reach limit
                if server.buffer.len() >= BUFFER_FLUSH_THRESHOLD {
                    break;
                }
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
                let _ = server.registering_prepared_statement.pop_front();
                if server.is_async() {
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
    }

    // zero-copy hand-off. `BytesMut::clone()` would deep-copy every
    // byte of the response; `split()` takes ownership of the filled bytes
    // and leaves the capacity behind for the next call - same effect as
    // clone+clear without the alloc+memcpy. At 100k qps with multi-KiB
    // responses this saves hundreds of MB/s of allocator + memcpy work.
    let bytes = if server.buffer.len() > BUFFER_FLUSH_THRESHOLD {
        // Buffer outgrew the configured per-server cap. Take the whole
        // payload and let the buffer re-acquire fresh BUFFER_FLUSH_THRESHOLD
        // capacity on the next push - bounds the long-tail memory of a
        // chatty backend.

        std::mem::replace(
            &mut server.buffer,
            BytesMut::with_capacity(BUFFER_FLUSH_THRESHOLD),
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
    use crate::server::cleanup::ResetCleanupCommand;
    use ahash::RandomState;
    use bytes::BytesMut;
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
        server
            .registering_prepared_statement
            .push_back("DOORMAN_bad".to_string());

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
            impl_src.contains("write_all_flush_timeout(client_stream, &server.buffer"),
            "large-message header flushes to the client must be bounded by proxy_copy_data_timeout"
        );
        assert!(
            !impl_src.contains("write_all_flush(client_stream, &server.buffer)"),
            "large-message handlers must not flush headers with an unbounded client write"
        );
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
