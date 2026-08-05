use bytes::{BufMut, BytesMut};
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::ops::DerefMut;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use crate::utils::clock::now;

use crate::admin::handle_admin;
#[cfg(unix)]
use crate::app::server::MIGRATION_NOTIFY;
use crate::app::server::{
    migration_in_progress, CLIENTS_IN_TRANSACTIONS, MIGRATION_TX, SHUTDOWN_IN_PROGRESS,
};
use crate::client::batch_handling::PARSE_COMPLETE_MSG;
use crate::client::core::{BatchOperation, Client, PreparedStatementKey, SkippedParse};
use crate::client::util::{
    contains_discard_all, extract_deallocate_target, extract_set_and_reset_cleanup_commands,
    is_standalone_begin, simple_query_body, simple_query_starts_with_prepare, QUERY_DEALLOCATE,
};
use crate::config::config_arc;
use crate::errors::Error;
use crate::messages::{
    ends_with_idle_ready_for_query, error_response_timeout, has_error_response,
    insert_close_complete_after_last_close_complete, read_message_reuse,
    read_message_reuse_cancel_safe, simple_query, write_all_flush_timeout, Parse,
};
use crate::pool::{canceled_pids_consume, CancelMarker};
use crate::server::{
    AsyncExpectedResponse, Server, SetResponseOutcome, SyncPlan, HOUSEKEEPING_TIMEOUT,
};
use crate::utils::buffering_writer::BufferingWriter;
use crate::utils::debug_messages::{log_client_to_server, log_server_to_client};
use crate::web::metrics::{
    POOLER_CHECK_QUERY_BACKEND_TOTAL, POOLER_CHECK_QUERY_CACHE_TOTAL,
    POOLER_CHECK_QUERY_OVERSIZE_TOTAL,
};
use tokio::io::AsyncBufReadExt;

const POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

fn checkout_resource_exhausted_client_message(_detail: &str) -> &'static str {
    "Connection pooler local resource exhausted. Please try again later."
}

fn append_pooler_check_query_response(response: &mut BytesMut, bytes: &[u8]) -> Result<(), String> {
    let next_len = response
        .len()
        .checked_add(bytes.len())
        .ok_or_else(|| "pooler_check_query response length overflow".to_string())?;
    if next_len > POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES {
        return Err(format!(
            "pooler_check_query response exceeded {POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES} byte limit"
        ));
    }
    response.extend_from_slice(bytes);
    Ok(())
}

fn is_pooler_check_query_oversize_recv_error(err: &Error) -> bool {
    match err {
        Error::MaxMessageSize => true,
        Error::SocketError(msg) => msg.contains("buffering writer cap exceeded"),
        _ => false,
    }
}

#[cfg(unix)]
enum IdleClientRead {
    Message(BytesMut),
    MigrationRequested,
}

#[cfg(unix)]
async fn read_idle_message_or_migration_notice<S>(
    read: &mut tokio::io::BufReader<S>,
    read_buf: &mut BytesMut,
    max_memory_usage: u64,
    admin: bool,
    migration_wake_enabled: bool,
) -> Result<IdleClientRead, Error>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
{
    if admin || !migration_wake_enabled {
        return read_message_reuse_cancel_safe(read, read_buf, max_memory_usage)
            .await
            .map(IdleClientRead::Message);
    }

    loop {
        enum ReadRace {
            Message(Result<BytesMut, Error>),
            MigrationNotice,
        }

        let race = {
            let migration_notice = MIGRATION_NOTIFY.notified();
            tokio::pin!(migration_notice);
            migration_notice.as_mut().enable();

            if migration_in_progress() && read_buf.is_empty() && read.buffer().is_empty() {
                return Ok(IdleClientRead::MigrationRequested);
            }

            let read_next = read_message_reuse_cancel_safe(read, read_buf, max_memory_usage);
            tokio::pin!(read_next);

            tokio::select! {
                biased;
                result = &mut read_next => {
                    ReadRace::Message(result)
                }
                _ = &mut migration_notice => {
                    ReadRace::MigrationNotice
                }
            }
        };

        match race {
            ReadRace::Message(result) => return result.map(IdleClientRead::Message),
            ReadRace::MigrationNotice => {
                if migration_in_progress() && read_buf.is_empty() && read.buffer().is_empty() {
                    return Ok(IdleClientRead::MigrationRequested);
                }
            }
        }
    }
}

// =============================================================================
// PostgreSQL Extended Query Protocol - Documentation
// =============================================================================
//
// This module handles the PostgreSQL Extended Query Protocol, which allows
// clients to send multiple messages in a batch before requesting results.
//
// ## Protocol Message Types (Client → Server)
//
// | Code | Message      | Description                                      |
// |------|--------------|--------------------------------------------------|
// | 'P'  | Parse        | Prepare a statement (with optional name)         |
// | 'B'  | Bind         | Bind parameters to a prepared statement          |
// | 'E'  | Execute      | Execute a bound portal                           |
// | 'D'  | Describe     | Request description of statement or portal       |
// | 'C'  | Close        | Close a prepared statement or portal             |
// | 'S'  | Sync         | Synchronization point, requests results          |
// | 'H'  | Flush        | Request server to flush output (async mode)      |
// | 'Q'  | Query        | Simple query (not extended protocol)             |
// | 'F'  | FunctionCall | Fastpath function call                           |
// | 'X'  | Terminate    | Close connection                                 |
//
// ## Protocol Message Types (Server → Client)
//
// | Code | Message              | Description                              |
// |------|----------------------|------------------------------------------|
// | '1'  | ParseComplete        | Statement was parsed successfully        |
// | '2'  | BindComplete         | Parameters were bound successfully       |
// | 'T'  | RowDescription       | Description of result columns            |
// | 'D'  | DataRow              | A row of query results                   |
// | 'C'  | CommandComplete      | Command finished (with row count)        |
// | 't'  | ParameterDescription | Description of statement parameters      |
// | 'n'  | NoData               | Statement returns no data                |
// | '3'  | CloseComplete        | Statement/portal was closed              |
// | 'Z'  | ReadyForQuery        | Server ready for next query              |
// | 'V'  | FunctionCallResponse | Fastpath function result                 |
// | 'E'  | ErrorResponse        | An error occurred                        |
//
// ## Basic Extended Query Flow
//
// ```text
// Client                      Proxy                      Server
//   │                           │                           │
//   │──── Parse (P) ───────────>│                           │
//   │──── Bind (B) ────────────>│                           │
//   │──── Execute (E) ─────────>│                           │
//   │──── Sync (S) ────────────>│──── P,B,E,S ────────────>│
//   │                           │                           │
//   │                           │<─── ParseComplete (1) ────│
//   │                           │<─── BindComplete (2) ─────│
//   │                           │<─── DataRow... (D) ───────│
//   │                           │<─── CommandComplete (C) ──│
//   │<──── Response ────────────│<─── ReadyForQuery (Z) ────│
// ```
//
// ## Prepared Statement Caching
//
// pg_doorman caches prepared statements to avoid re-parsing identical queries.
// When a Parse message arrives:
//
// 1. If statement is NOT in cache → send Parse to server, cache it
// 2. If statement IS in cache AND server has it → skip Parse, inject ParseComplete
// 3. If statement IS in cache BUT server doesn't have it → send Parse to server
//
// ## Batch Processing with Cached Statements
//
// When some Parse messages are skipped (cached), we must inject ParseComplete
// responses in the correct order. Example:
//
// ```text
// Client sends:              Server receives:         Server responds:
// ┌─────────────────┐        ┌─────────────────┐      ┌─────────────────┐
// │ Parse "stmt1"   │──┐     │                 │      │                 │
// │ (cached,skip)   │  │     │                 │      │                 │
// ├─────────────────┤  │     ├─────────────────┤      ├─────────────────┤
// │ Parse "stmt2"   │──┼────>│ Parse "stmt2"   │─────>│ ParseComplete   │
// │ (new, send)     │  │     │                 │      │                 │
// ├─────────────────┤  │     ├─────────────────┤      ├─────────────────┤
// │ Bind to "stmt1" │──┼────>│ Bind to "stmt1" │─────>│ BindComplete    │
// ├─────────────────┤  │     ├─────────────────┤      ├─────────────────┤
// │ Sync            │──┘────>│ Sync            │─────>│ ReadyForQuery   │
// └─────────────────┘        └─────────────────┘      └─────────────────┘
//
// Proxy must reorder response to client:
// ┌─────────────────┐
// │ ParseComplete   │ ← injected for skipped "stmt1"
// │ ParseComplete   │ ← from server for "stmt2"
// │ BindComplete    │ ← from server
// │ ReadyForQuery   │ ← from server
// └─────────────────┘
// ```
//
// The `reorder_parse_complete_responses()` function handles this reordering
// by tracking batch operations and inserting synthetic ParseComplete messages
// at the correct positions in the response stream.
//
// ## Async Mode (Flush command)
//
// When client uses 'H' (Flush) instead of 'S' (Sync), it enters async mode.
// In async mode, prepared statement caching is disabled to avoid
// "prepared statement already exists" errors, because the client may
// send multiple Parse messages for the same statement before receiving
// responses.
//
// =============================================================================

/// Buffer flush threshold in bytes (8 KiB).
/// When the buffer reaches this size, it will be flushed to avoid excessive memory usage.
const BUFFER_FLUSH_THRESHOLD: usize = 8192;

/// hard cap on the per-client extended-protocol pending
/// buffer (`self.buffer`). Without this, an authenticated client can
/// drive the pooler to OOM by pipelining Parse/Bind/Execute/Close
/// without ever sending Sync (`S`) or Flush (`H`) - `self.buffer` is
/// only cleared after a successful round-trip. The cap is enforced
/// inside each extended-protocol handler; on overflow the connection
/// is closed and the backend marked bad (the inner-handler action
/// match maps the Err to `server.mark_bad` before propagating).
///
/// Equals `MAX_MESSAGE_SIZE` (256MB), the read-side ceiling for one wire
/// message: a legitimate single large Bind (a file-upload bytea parameter
/// arrives as one Bind followed by Sync) always fits, and only pipelining
/// without Sync/Flush can overflow the cap.
pub(crate) const EXTENDED_BATCH_BUFFER_CAP: usize = crate::messages::MAX_MESSAGE_SIZE as usize;

/// shared check for every extended-protocol handler before
/// it appends to `self.buffer`. Returns an Err that propagates through
/// the inner-handler `action_result` -> `server.mark_bad` path, so the
/// backend is evicted and the client connection is closed.
#[inline]
pub(crate) fn enforce_extended_batch_buffer_cap(
    current_len: usize,
    incoming_len: usize,
    location: &'static str,
) -> Result<(), crate::app::errors::Error> {
    if current_len.saturating_add(incoming_len) > EXTENDED_BATCH_BUFFER_CAP {
        return Err(crate::app::errors::Error::ClientError(format!(
            "extended-protocol pending buffer would exceed {EXTENDED_BATCH_BUFFER_CAP} bytes \
             (current={current_len}, +{incoming_len}, at {location}) - \
             client did not Sync/Flush in time"
        )));
    }
    Ok(())
}

/// Cap retained extended-protocol batch metadata that is not necessarily
/// represented in `self.buffer`. Cached Parse skips append metadata but no
/// backend-bound bytes, so the wire-buffer cap alone cannot bound memory.
/// Deliberately NOT tied to `EXTENDED_BATCH_BUFFER_CAP`: metadata grows only
/// with the NUMBER of pipelined operations (tens of bytes each), never with
/// payload size, so a large-payload allowance has no reason to loosen it.
pub(crate) const EXTENDED_BATCH_METADATA_CAP: usize = 16 * 1024 * 1024;

#[inline]
fn extended_batch_metadata_bytes(batch_operations: usize, skipped_parses: usize) -> usize {
    let batch_bytes = batch_operations.saturating_mul(std::mem::size_of::<BatchOperation>());
    let skipped_bytes = skipped_parses.saturating_mul(
        std::mem::size_of::<SkippedParse>().saturating_add(PARSE_COMPLETE_MSG.len()),
    );
    batch_bytes.saturating_add(skipped_bytes)
}

#[inline]
pub(crate) fn enforce_extended_batch_metadata_cap(
    current_batch_operations: usize,
    current_skipped_parses: usize,
    incoming_batch_operations: usize,
    incoming_skipped_parses: usize,
    location: &'static str,
) -> Result<(), crate::app::errors::Error> {
    let current = extended_batch_metadata_bytes(current_batch_operations, current_skipped_parses);
    let incoming =
        extended_batch_metadata_bytes(incoming_batch_operations, incoming_skipped_parses);
    if current.saturating_add(incoming) > EXTENDED_BATCH_METADATA_CAP {
        return Err(crate::app::errors::Error::ClientError(format!(
            "extended-protocol pending metadata would exceed {EXTENDED_BATCH_METADATA_CAP} bytes \
             (current={current}, +{incoming}, at {location}) - \
             client did not Sync/Flush in time"
        )));
    }
    Ok(())
}

/// synthetic acknowledgement for a SIMPLE-query
/// `DEALLOCATE <name>` that targets a statement pg_doorman renamed on
/// the wire during the EXTENDED protocol (`Parse "S_1"` ->
/// `DOORMAN_<n>`). Such a name cannot be forwarded verbatim: the
/// backend only knows the `DOORMAN_<n>` alias and would answer
/// SQLSTATE 26000 `prepared statement "S_1" does not exist`. The F3
/// commit (a5187fb) made the simple-query DEALLOCATE forward verbatim
/// to fix a `PREPARE x; DEALLOCATE x; PREPARE x` 42P05 regression for
/// SQL-level PREPARE names - but that regressed the extended-renamed
/// case. We restore the synthetic ack ONLY for names pg_doorman
/// actually renamed (identified by a hit in the per-client cache),
/// while SQL-level / unknown names keep forwarding verbatim.
///
/// Wire layout (22 bytes):
///   CommandComplete: 'C' + i32(15) + "DEALLOCATE\0"
///   ReadyForQuery:   'Z' + i32(5)  + 'I' (idle)
pub(crate) const SIMPLE_DEALLOCATE_NAMED_ACK: [u8; 22] = [
    b'C', 0, 0, 0, 15, b'D', b'E', b'A', b'L', b'L', b'O', b'C', b'A', b'T', b'E', 0, b'Z', 0, 0,
    0, 5, b'I',
];

/// outcome of the forward/synthesize decision for a SIMPLE-query
/// `DEALLOCATE <name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeallocateForwardAction {
    /// Answer the client directly with `SIMPLE_DEALLOCATE_NAMED_ACK`
    /// (the name was an extended-renamed client statement; forwarding
    /// verbatim would 26000).
    SynthesizeAck,
    /// Forward the verbatim `DEALLOCATE <name>` to the backend (SQL-level
    /// PREPARE name, unknown name, or prepared statements disabled).
    Forward,
}

/// pure decision for a `DEALLOCATE <name>`.
///
/// `was_cached_client_statement` is `true` when the per-client prepared
/// statement cache held an entry under the client-supplied name - which
/// is exactly the set of names pg_doorman renamed to `DOORMAN_<n>` on
/// the wire. Those must be acked synthetically (forwarding verbatim
/// would 26000). Everything else forwards verbatim, preserving the F3
/// fix for SQL-level PREPARE names. When prepared statements are
/// disabled nothing was ever renamed, so always forward.
pub(crate) fn simple_deallocate_action(
    was_cached_client_statement: bool,
    prepared_statements_enabled: bool,
) -> DeallocateForwardAction {
    if prepared_statements_enabled && was_cached_client_statement {
        DeallocateForwardAction::SynthesizeAck
    } else {
        DeallocateForwardAction::Forward
    }
}

#[inline]
pub(crate) fn non_extended_protocol_can_forward(
    pending_buffer_len: usize,
    pending_batch_operations: usize,
    pending_skipped_parses: usize,
) -> bool {
    pending_buffer_len == 0 && pending_batch_operations == 0 && pending_skipped_parses == 0
}

/// RAII guard for CLIENTS_IN_TRANSACTIONS counter.
/// Increments on creation, decrements on drop.
struct TransactionGuard;

impl TransactionGuard {
    fn new() -> Self {
        CLIENTS_IN_TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for TransactionGuard {
    fn drop(&mut self) {
        CLIENTS_IN_TRANSACTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Result of waiting for the next client message while monitoring server liveness.
enum NextClientMessage {
    Message(BytesMut),
    ServerDead,
}

/// Action to take after processing a message in the transaction loop
enum TransactionAction {
    /// Continue processing messages in the transaction loop
    Continue,
    /// Break out of the transaction loop (release server)
    Break,
}

enum DeferredExtendedBeginState {
    Parsed {
        parse: BytesMut,
    },
    Bound {
        parse: BytesMut,
        bind: BytesMut,
    },
    Executed {
        parse: BytesMut,
        bind: BytesMut,
        execute: BytesMut,
    },
}

enum PreServerMessageAction {
    Handled,
    Process(BytesMut),
}

impl DeferredExtendedBeginState {
    fn replay_with(self, current: BytesMut, replay: &mut VecDeque<BytesMut>) {
        match self {
            DeferredExtendedBeginState::Parsed { parse } => {
                replay.push_back(parse);
            }
            DeferredExtendedBeginState::Bound { parse, bind } => {
                replay.push_back(parse);
                replay.push_back(bind);
            }
            DeferredExtendedBeginState::Executed {
                parse,
                bind,
                execute,
            } => {
                replay.push_back(parse);
                replay.push_back(bind);
                replay.push_back(execute);
            }
        }
        replay.push_back(current);
    }
}

fn simple_begin_message() -> BytesMut {
    BytesMut::from(&b"Q\0\0\0\x0bBEGIN;\0"[..])
}

fn is_plain_begin_sql(query: &str) -> bool {
    query
        .trim()
        .trim_end_matches(';')
        .trim()
        .eq_ignore_ascii_case("begin")
}

fn is_extended_begin_parse(message: &BytesMut) -> bool {
    if message.first() != Some(&b'P') {
        return false;
    }
    match Parse::try_from(message) {
        Ok(parse) => {
            parse.name.is_empty() && parse.num_params() == 0 && is_plain_begin_sql(parse.query())
        }
        Err(_) => false,
    }
}

fn is_empty_unnamed_bind(message: &BytesMut) -> bool {
    message.len() == 13
        && message[0] == b'B'
        && message[1..5] == [0, 0, 0, 12]
        && message[5..13] == [0; 8]
}

fn is_empty_portal_execute(message: &BytesMut) -> bool {
    message.len() == 10
        && message[0] == b'E'
        && message[1..5] == [0, 0, 0, 9]
        && message[5] == 0
        && message[6..10] == [0; 4]
}

fn is_sync_message(message: &BytesMut) -> bool {
    message.len() == 5 && message[0] == b'S' && message[1..5] == [0, 0, 0, 4]
}

#[inline(always)]
fn should_release_transaction_backend(
    transaction_mode: bool,
    transaction_complete: bool,
    sql_prepare_session_pinned: bool,
) -> bool {
    transaction_mode && transaction_complete && !sql_prepare_session_pinned
}

fn response_contains_sql_prepare_command_complete(response: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos < response.len() {
        if response.len() - pos < 5 {
            return false;
        }

        let tag = response[pos];
        let len = u32::from_be_bytes([
            response[pos + 1],
            response[pos + 2],
            response[pos + 3],
            response[pos + 4],
        ]) as usize;
        if len < 4 {
            return false;
        }

        let Some(frame_end) = pos.checked_add(1).and_then(|start| start.checked_add(len)) else {
            return false;
        };
        if frame_end > response.len() {
            return false;
        }

        if tag == b'C' && &response[pos + 5..frame_end] == b"PREPARE\0" {
            return true;
        }

        pos = frame_end;
    }

    false
}

fn take_queued_pre_server_message(
    initial_message: &mut Option<BytesMut>,
    replay: &mut VecDeque<BytesMut>,
) -> Option<BytesMut> {
    initial_message.take().or_else(|| replay.pop_front())
}

fn backend_timeout_client_error(err: &Error) -> Option<(&'static str, &'static str)> {
    match err {
        Error::FlushTimeout => Some((
            "pooler is shut down now (flush timeout: server did not accept data within the timeout period)",
            "58006",
        )),
        Error::SocketError(msg) if msg == "timeout waiting for COPY completion response" => Some((
            "pooler is shut down now (COPY completion timeout: server did not finish within the timeout period)",
            "58006",
        )),
        Error::SocketError(msg)
            if msg == "timeout sending deferred BEGIN"
                || msg == "timeout waiting for deferred BEGIN response" =>
        {
            Some((
                "pooler is shut down now (deferred BEGIN timeout: server did not finish within the timeout period)",
                "58006",
            ))
        }
        _ => None,
    }
}

impl<S, T> Client<S, T>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    #[inline(always)]
    fn complete_transaction_if_needed(&mut self, server: &Server, check_async: bool) -> bool {
        if server.in_transaction() {
            if self.session_xact_start.is_none() {
                self.session_xact_start = Some(crate::utils::clock::now());
            }
            return false;
        }

        self.stats.transaction();
        server
            .stats
            .transaction(self.server_parameters.get_application_name());

        if !self.transaction_mode {
            if let Some(start) = self.session_xact_start.take() {
                server
                    .stats
                    .add_xact_time_and_idle(start.elapsed().as_micros() as u64);
            }
        }

        let transaction_complete = !server.in_copy_mode() && (!check_async || !server.is_async());
        if transaction_complete {
            self.prepared.clear_portal_cleanup_commands();
        }

        if should_release_transaction_backend(
            self.transaction_mode,
            transaction_complete,
            self.sql_prepare_session_pinned,
        ) {
            return true;
        }

        false
    }

    /// Ensure server is in copy mode, return error if not
    #[inline(always)]
    fn ensure_copy_mode(&mut self, server: &mut Server) -> Result<(), Error> {
        if !server.in_copy_mode() {
            self.stats.disconnect();
            server.mark_bad("client expects COPY mode but server is not in COPY mode");
            return Err(Error::ProtocolSyncError(
                "server not in copy mode".to_string(),
            ));
        }
        Ok(())
    }

    /// Wait for the next client message while monitoring server connection liveness.
    ///
    /// This method is called on **every** iteration of the transaction loop —
    /// for each SQL statement inside a `BEGIN ... COMMIT` block.  A typical
    /// ORM or batch client sends `BEGIN`, then 3-10 queries with 1-5 ms
    /// round-trip between them, then `COMMIT`.  Using `tokio::select!` with
    /// two sockets on every call doubles the epoll syscall overhead and
    /// measurably degrades throughput (5-10 % on real benchmarks).
    ///
    /// Three-level strategy keeps the hot path fast:
    ///
    /// 1. **Instant check** (`poll_fn`): single poll — if data is already in
    ///    the read buffer (common on localhost or when the client pipelines),
    ///    return immediately.  Zero extra syscalls, zero timer overhead.
    ///
    /// 2. **Short wait** (`timeout 100 ms`): covers real-world clients with
    ///    1-50 ms network round-trip.  `tokio::time::timeout` inserts one
    ///    entry into the in-memory timer wheel — no syscall, nanosecond cost.
    ///    The vast majority of transactional traffic completes here.
    ///
    /// 3. **Full monitor** (`select!`): client is truly idle (> 100 ms) — now
    ///    worth paying for the second epoll interest to race client read
    ///    against `server_readable()`.  Detects dead servers (e.g.
    ///    `pg_terminate_backend`, `idle_in_transaction_session_timeout`) and
    ///    releases the pool slot early instead of holding it indefinitely.
    async fn wait_for_next_message(&mut self, server: &Server) -> Result<NextClientMessage, Error> {
        let mut read_fut = std::pin::pin!(read_message_reuse(
            &mut self.read,
            &mut self.read_buf,
            self.max_memory_usage
        ));

        let instant = poll_fn(|cx| match read_fut.as_mut().poll(cx) {
            Poll::Ready(result) => Poll::Ready(Some(result)),
            Poll::Pending => Poll::Ready(None),
        })
        .await;

        if let Some(result) = instant {
            return result.map(NextClientMessage::Message);
        }

        if let Ok(result) = tokio::time::timeout(Duration::from_millis(100), &mut read_fut).await {
            return result.map(NextClientMessage::Message);
        }

        loop {
            tokio::select! {
                biased;
                result = &mut read_fut => {
                    return result.map(NextClientMessage::Message);
                }
                _ = server.server_readable() => {
                    if server.check_server_alive() {
                        continue;
                    }
                    return Ok(NextClientMessage::ServerDead);
                }
            }
        }
    }

    /// Handle cancel mode - when client wants to cancel a earlier issued query.
    /// Opens a new separate connection to the server, sends the backend_id
    /// and secret_key and then closes it for security reasons.
    async fn handle_cancel_mode(&self) -> Result<(), Error> {
        let target = match self
            .client_server_map
            .get(&(self.connection_id as i32, self.secret_key))
        {
            // We found the server the client is using for its query
            // that it wants to cancel.
            Some(entry) => {
                let t = entry.value();
                // shard-local insert into DashSet.
                //
                // fail-closed. `should_forward_cancel` sets the
                // quarantine marker (`CANCELED_PIDS`) and reports whether
                // the cancel may be forwarded. The marker is what keeps a
                // forwarded cancel safe in `pool_mode=transaction`: a
                // backend whose pid is quarantined is evicted on check-in
                // instead of being recycled to the next client. If the
                // set is at `CANCELED_PIDS_CAP` the marker is NOT set, so
                // forwarding the cancel would let the backend be handed to
                // a different client before the async cancel TCP lands -
                // cancelling that other client's query (cross-client
                // cancel). Drop the cancel instead: the client's Ctrl-C
                // becomes a safe no-op, which is strictly better than
                // cancelling someone else's work.
                if !crate::pool::should_forward_cancel(t.process_id) {
                    warn!(
                        "[{}@{} #c{}] dropping cancel for backend pid={} - CANCELED_PIDS at cap, \
                         quarantine marker not set; forwarding would risk a cross-client cancel",
                        self.username, self.pool_name, self.connection_id, t.process_id
                    );
                    return Ok(());
                }
                // surface per-pool cancel counter in SHOW POOLS.
                // Look up the pool by identifier; if the pool was reloaded
                // away while a cancel was in flight, skip the counter
                // bump silently - the cancel still goes through.
                if let Some(pool) = crate::pool::get_pool_by_id(&self.cached_pool_id) {
                    pool.address.stats.cancel_request();
                }
                t.clone()
            }

            // The client doesn't know / got the wrong server,
            // we're closing the connection for security reasons.
            None => return Ok(()),
        };

        Server::cancel(
            &target.host,
            target.port,
            target.process_id,
            target.secret_key,
            &target.server_tls,
            target.connected_with_tls,
            &target.pool_name,
        )
        .await
    }

    async fn try_handle_deferred_extended_begin(
        &mut self,
        message: BytesMut,
        state: &mut Option<DeferredExtendedBeginState>,
        replay: &mut VecDeque<BytesMut>,
    ) -> Result<PreServerMessageAction, Error> {
        match state.take() {
            None => {
                if self.client_pending_begin.is_none() && is_extended_begin_parse(&message) {
                    *state = Some(DeferredExtendedBeginState::Parsed { parse: message });
                    return Ok(PreServerMessageAction::Handled);
                }
                Ok(PreServerMessageAction::Process(message))
            }
            Some(DeferredExtendedBeginState::Parsed { parse }) => {
                if is_empty_unnamed_bind(&message) {
                    *state = Some(DeferredExtendedBeginState::Bound {
                        parse,
                        bind: message,
                    });
                    return Ok(PreServerMessageAction::Handled);
                }
                DeferredExtendedBeginState::Parsed { parse }.replay_with(message, replay);
                Ok(PreServerMessageAction::Handled)
            }
            Some(DeferredExtendedBeginState::Bound { parse, bind }) => {
                if is_empty_portal_execute(&message) {
                    *state = Some(DeferredExtendedBeginState::Executed {
                        parse,
                        bind,
                        execute: message,
                    });
                    return Ok(PreServerMessageAction::Handled);
                }
                DeferredExtendedBeginState::Bound { parse, bind }.replay_with(message, replay);
                Ok(PreServerMessageAction::Handled)
            }
            Some(DeferredExtendedBeginState::Executed {
                parse,
                bind,
                execute,
            }) => {
                if is_sync_message(&message) {
                    const SYNTHETIC_EXTENDED_BEGIN_RESPONSE: &[u8] = &[
                        b'1', 0, 0, 0, 4, // ParseComplete
                        b'2', 0, 0, 0, 4, // BindComplete
                        b'C', 0, 0, 0, 10, b'B', b'E', b'G', b'I', b'N',
                        0, // CommandComplete("BEGIN")
                        b'Z', 0, 0, 0, 5, b'T', // ReadyForQuery('T')
                    ];
                    let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
                    write_all_flush_timeout(
                        &mut self.write,
                        SYNTHETIC_EXTENDED_BEGIN_RESPONSE,
                        write_timeout,
                    )
                    .await?;
                    self.client_pending_begin = Some(simple_begin_message());
                    return Ok(PreServerMessageAction::Handled);
                }
                DeferredExtendedBeginState::Executed {
                    parse,
                    bind,
                    execute,
                }
                .replay_with(message, replay);
                Ok(PreServerMessageAction::Handled)
            }
        }
    }

    /// Handle pooler health checks, standalone DISCARD ALL and eligible
    /// DEALLOCATE queries before checking out a backend.
    /// Returns `Ok(true)` if query was handled (caller should continue to next iteration),
    /// `Ok(false)` if query needs normal processing.
    #[inline]
    async fn try_handle_without_server(
        &mut self,
        message: &BytesMut,
        pool: &crate::pool::ConnectionPool,
        query_start_at: quanta::Instant,
    ) -> Result<bool, Error> {
        if message[0] != b'Q' {
            return Ok(false);
        }

        // Pooler health-check query — byte-for-byte match against the
        // pre-encoded `general.pooler_check_query`. The same snapshot is
        // used as the cache key in `handle_pooler_check_query`, so a
        // RELOAD that races with an in-flight probe can never mix
        // request bytes from one config with a cache key from another.
        //
        // `load()` returns a lock-free Guard
        // and skips the atomic refcount increment that `load_full()`
        // would emit on every simple query. The Guard is stable across
        // the await below - ArcSwap holds the old Arc alive until every
        // borrow is dropped - so the byte comparison and the downstream
        // `handle_pooler_check_query(..., &snapshot)` see the same
        // memory the comparison succeeded against.
        let snapshot = crate::config::POOLER_CHECK_QUERY_SNAPSHOT.load();
        if message.len() == snapshot.request_bytes.len()
            && snapshot.request_bytes.as_ref() == &message[..]
        {
            self.handle_pooler_check_query(message, pool, &snapshot)
                .await?;
            return Ok(true);
        }

        if self.transaction_mode
            && !self.sql_prepare_session_pinned
            && pool.settings.intercept_discard_all
            && contains_discard_all(simple_query_body(message))
        {
            self.stats.active_idle();
            self.respond_to_simple_discard(false).await?;

            let elapsed_us = query_start_at.elapsed().as_micros() as u64;
            self.stats.query();
            self.stats.transaction();
            pool.address.stats.query_count_add();
            pool.address.stats.query_time_add_microseconds(elapsed_us);
            pool.address.stats.xact_count_add();
            pool.address.stats.xact_time_add(elapsed_us);
            crate::web::metrics::observe_pool_query_microseconds(
                &pool.address.username,
                &pool.address.pool_name,
                elapsed_us,
            );
            crate::web::metrics::observe_pool_transaction_microseconds(
                &pool.address.username,
                &pool.address.pool_name,
                elapsed_us,
            );
            pool.address.stats.discard_all_intercepted();
            self.stats.idle_read();
            return Ok(true);
        }

        // Check for DEALLOCATE query and clear client prepared statements cache.
        // Format: Q message = [Q:1][length:4][query][null:1]
        // QUERY_DEALLOCATE = "deallocate " (11 bytes).
        //
        // Driver-generated statement names and observability
        // comments can make valid DEALLOCATE frames much larger than the
        // SQL keyword itself. Do not apply a separate total-frame heuristic
        // here; the SimpleQuery reader already enforces the protocol memory
        // cap, and the strict SQL parser below rejects non-DEALLOCATE bodies.
        if message.len() > QUERY_DEALLOCATE.len() + 6 {
            let query_bytes = simple_query_body(message);

            // tokenizing parser (skips leading whitespace,
            // line `--` and block `/* */` comments, and the optional
            // `PREPARE` keyword) - replaces the old literal prefix
            // check that silently missed `/* hint */ DEALLOCATE foo`
            // shapes used by sqlcommenter / Datadog APM / pgbench
            // script preambles.
            if let Some(target) = crate::client::util::extract_deallocate_target(query_bytes) {
                // update client-side cache, then
                // **forward** the DEALLOCATE to the backend instead of
                // synthesising a CommandComplete. A synthetic ack would make
                // `try_handle_without_server` lie to the client:
                // pg_doorman dropped its per-client cache entry, but the
                // backend still held the prepared statement, so the next
                // simple-query `PREPARE <name>` (possibly on a different
                // pool checkout in transaction mode) erred with SQLSTATE
                // 42P05 `prepared statement "<name>" already exists`.
                // Forwarding is the correct behaviour: the backend either
                // deletes the name (simple-query `PREPARE`-created) or
                // returns SQLSTATE 26000 `does not exist` (extended-Parse
                // path where pg_doorman renamed the statement to
                // `DOORMAN_<n>`) - both of which are honest signals the
                // client can act on. The client-side cache update happens
                // first so a backend ERROR does not leave stale state.
                match target {
                    crate::client::util::DeallocateTarget::All => {
                        let count = self.prepared.discard_clear();
                        self.update_prepared_cache_stats();
                        info!(
                            "[{}@{} #c{}] DEALLOCATE ALL: cleared {} entries from client cache; forwarding to backend",
                            self.username, self.pool_name, self.connection_id, count
                        );
                    }
                    crate::client::util::DeallocateTarget::Named(name) => {
                        let key = PreparedStatementKey::Named(name.clone());
                        // A cache hit means this name is one pg_doorman
                        // renamed to `DOORMAN_<n>` during the EXTENDED
                        // protocol. The backend does not know it under the
                        // client's name, so forwarding the verbatim
                        // `DEALLOCATE <name>` would answer SQLSTATE 26000.
                        let was_cached_client_statement = self.prepared.cache.pop(&key).is_some();
                        if was_cached_client_statement {
                            self.update_prepared_cache_stats();
                        }
                        // decide whether to ack synthetically (the
                        // name was extended-renamed) or forward verbatim
                        // (SQL-level PREPARE / unknown name - preserve the
                        // Fix for `PREPARE x; DEALLOCATE x; PREPARE x`).
                        match simple_deallocate_action(
                            was_cached_client_statement,
                            self.prepared.enabled,
                        ) {
                            DeallocateForwardAction::SynthesizeAck => {
                                debug!(
                                    "[{}@{} #c{}] DEALLOCATE {}: extended-renamed client statement; \
                                     answering with synthetic ack (backend only knows DOORMAN_<n>)",
                                    self.username, self.pool_name, self.connection_id, name
                                );
                                let write_timeout =
                                    config_arc().general.proxy_copy_data_timeout.as_std();
                                write_all_flush_timeout(
                                    &mut self.write,
                                    &SIMPLE_DEALLOCATE_NAMED_ACK,
                                    write_timeout,
                                )
                                .await?;
                                // Handled without touching a backend.
                                return Ok(true);
                            }
                            DeallocateForwardAction::Forward => {
                                debug!(
                                    "[{}@{} #c{}] DEALLOCATE {}: forwarding verbatim to backend",
                                    self.username, self.pool_name, self.connection_id, name
                                );
                            }
                        }
                    }
                }

                // Return Ok(false) so the caller runs the normal forward
                // path (checkout server, send the Q, relay the
                // CommandComplete/ErrorResponse + ReadyForQuery back).
                return Ok(false);
            }
        }

        Ok(false)
    }

    fn track_forwarded_simple_deallocate_cache_state(&mut self, message: &BytesMut) {
        if message.len() <= QUERY_DEALLOCATE.len() + 6 {
            return;
        }

        let Some(target) = extract_deallocate_target(simple_query_body(message)) else {
            return;
        };

        match target {
            crate::client::util::DeallocateTarget::All => {
                let count = self.prepared.discard_clear();
                self.update_prepared_cache_stats();
                debug!(
                    "[{}@{} #c{}] forwarded DEALLOCATE ALL: cleared {} entries from client cache",
                    self.username, self.pool_name, self.connection_id, count
                );
            }
            crate::client::util::DeallocateTarget::Named(name) => {
                let key = PreparedStatementKey::Named(name.clone());
                if self.prepared.cache.pop(&key).is_some() {
                    self.update_prepared_cache_stats();
                    debug!(
                        "[{}@{} #c{}] forwarded DEALLOCATE {}: removed client prepared cache entry",
                        self.username, self.pool_name, self.connection_id, name
                    );
                }
            }
        }
    }

    /// Serve a `general.pooler_check_query` SimpleQuery. The first probe in
    /// the pool's lifetime (and the first after a RELOAD that changes the
    /// value) forwards the query to PostgreSQL; subsequent probes answer
    /// from the per-pool response cache without touching the backend.
    /// `ErrorResponse` and any response that does not end in
    /// `ReadyForQuery('I')dle` are forwarded to the client as-is and
    /// never cached — caching them would freeze a non-idle backend state
    /// and replay it to later probes.
    async fn handle_pooler_check_query(
        &mut self,
        message: &BytesMut,
        pool: &crate::pool::ConnectionPool,
        snapshot: &crate::config::PoolerCheckQuerySnapshot,
    ) -> Result<(), Error> {
        if let Some(cached) = pool.check_query_cache.get(&snapshot.query) {
            POOLER_CHECK_QUERY_CACHE_TOTAL.inc();
            let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
            write_all_flush_timeout(&mut self.write, cached.as_ref(), write_timeout).await?;
            return Ok(());
        }

        let mut conn = loop {
            let mut conn = pool.database.get().await.map_err(|e| {
                Error::ClientError(format!(
                    "pooler_check_query: failed to acquire backend: {e}"
                ))
            })?;

            match canceled_pids_consume(conn.get_process_id()) {
                CancelMarker::Fresh => {
                    conn.mark_bad("pooler_check_query: connection was cancel-quarantined");
                    continue;
                }
                CancelMarker::Stale | CancelMarker::Absent => {}
            }

            if let Err(err) = conn.checkin_cleanup().await {
                conn.mark_bad(&format!(
                    "pooler_check_query: checkin_cleanup failed: {err}"
                ));
                return Err(err);
            }

            break conn;
        };

        let deadline = tokio::time::Instant::now() + HOUSEKEEPING_TIMEOUT;
        conn.begin_internal_round_trip();
        match tokio::time::timeout_at(deadline, conn.send_and_flush(message)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                conn.mark_bad(&format!("pooler_check_query: send failed: {err}"));
                return Err(err);
            }
            Err(_) => {
                conn.mark_bad("pooler_check_query: send timeout");
                return Err(Error::SocketError(
                    "timeout sending pooler_check_query".to_string(),
                ));
            }
        }
        POOLER_CHECK_QUERY_BACKEND_TOTAL.inc();

        // Server::recv must be drained in a loop until is_data_available()
        // is false; otherwise responses larger than BUFFER_FLUSH_THRESHOLD
        // leave bytes in the backend socket and the next checked-out client
        // reads a desynced stream.
        let mut response = BytesMut::new();
        let original_max_message_size = conn.max_message_size;
        conn.max_message_size = POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES.saturating_sub(1) as i32;
        loop {
            let mut overflow_buf = BytesMut::new();
            let writer = BufferingWriter::capped(
                &mut overflow_buf,
                POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES.saturating_sub(response.len()),
            );
            let chunk = match tokio::time::timeout_at(deadline, conn.recv(writer, None)).await {
                Ok(Ok(chunk)) => chunk,
                Ok(Err(err)) => {
                    conn.max_message_size = original_max_message_size;
                    if is_pooler_check_query_oversize_recv_error(&err) {
                        warn!(
                            "pooler_check_query response exceeded {POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES} byte limit while streaming; rejecting oversized backend response"
                        );
                        POOLER_CHECK_QUERY_OVERSIZE_TOTAL.inc();
                    }
                    conn.mark_bad(&format!("pooler_check_query: recv failed: {err}"));
                    return Err(err);
                }
                Err(_) => {
                    conn.max_message_size = original_max_message_size;
                    conn.mark_bad("pooler_check_query: recv timeout");
                    return Err(Error::SocketError(
                        "timeout waiting for pooler_check_query response".to_string(),
                    ));
                }
            };
            if let Err(err) = append_pooler_check_query_response(&mut response, &chunk) {
                conn.max_message_size = original_max_message_size;
                warn!("{err}; rejecting oversized pooler_check_query backend response");
                POOLER_CHECK_QUERY_OVERSIZE_TOTAL.inc();
                conn.mark_bad(&err);
                return Err(Error::SocketError(err));
            }
            if !overflow_buf.is_empty() {
                if let Err(err) = append_pooler_check_query_response(&mut response, &overflow_buf) {
                    conn.max_message_size = original_max_message_size;
                    warn!("{err}; rejecting oversized pooler_check_query backend response");
                    POOLER_CHECK_QUERY_OVERSIZE_TOTAL.inc();
                    conn.mark_bad(&err);
                    return Err(Error::SocketError(err));
                }
            }
            if !conn.is_data_available() {
                break;
            }
        }
        conn.max_message_size = original_max_message_size;
        conn.finish_internal_round_trip();
        // This internal checkout must end like a regular one: run the
        // checkin cleanup plus the release query so the release-cleanup
        // obligation armed at checkout is discharged before the Object
        // drops - otherwise the recycle-safety check would close a
        // perfectly healthy backend after every uncached probe.
        conn.finalize_checkin().await?;
        drop(conn);

        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        write_all_flush_timeout(&mut self.write, &response, write_timeout).await?;

        if !has_error_response(&response) && ends_with_idle_ready_for_query(&response) {
            pool.check_query_cache
                .set(snapshot.query.clone(), response.freeze());
        }

        Ok(())
    }

    /// Handle simple query (Q message).
    /// Returns the action to take after processing.
    #[inline]
    async fn handle_simple_query(
        &mut self,
        message: &BytesMut,
        server: &mut Server,
        query_start_at: quanta::Instant,
    ) -> Result<TransactionAction, Error> {
        // Simple query always ends with ReadyForQuery, so disable async mode
        // to wait for 'Z' instead of using expected_responses counter
        server.set_async_mode(false);
        server.set_expected_responses(0);

        // Defensively clear any pending extended-protocol attribution.
        // A simple query is opaque to the interner; whatever last_bound_for_top
        // held was from a prior extended batch and would otherwise leak its
        // hash into the next Sync.
        self.prepared.last_bound_for_top = None;

        self.track_forwarded_simple_deallocate_cache_state(message);

        let query_body = simple_query_body(message);
        let starts_sql_prepare = simple_query_starts_with_prepare(query_body);
        if self.transaction_mode && starts_sql_prepare {
            self.sql_prepare_session_pinned = true;
        }
        let (set_cleanup_commands, reset_cleanup_commands) =
            extract_set_and_reset_cleanup_commands(query_body);

        // piggyback a deferred `SET application_name` onto this
        // simple-query first message. At checkout the SyncPlan::AppNameOnly arm
        // stored the ready-to-send SET SQL in `pending_app_name_set` instead of
        // paying a standalone round-trip. We now concatenate TWO distinct `Q`
        // frames in ONE flush - the SET, then the client's own query - and
        // swallow exactly the SET's CommandComplete + ReadyForQuery before
        // relaying the client's response. PG pipelines two `Q` frames in one
        // read buffer natively (simple-query has no Sync). The frames are kept
        // separate (NOT merged into one "SET ...; <sql>" text).
        //
        // `take()` clears the slot on BOTH success and error paths (single
        // consumer per checkout; no stale carry across checkouts).
        if let Some(set_sql) = self.pending_app_name_set.take() {
            crate::web::metrics::inc_sync_params_plan("app_name_only", "simple_query_piggyback");

            // Mirror execute_server_roundtrip's session-mode xact-start init,
            // since this branch sends + relays directly (bypassing it). No-op
            // in transaction mode, where the piggyback common case lives.
            if !self.transaction_mode && self.session_xact_start.is_none() {
                self.session_xact_start = Some(crate::utils::clock::now());
            }

            // Time only the SET portion (flush -> swallow done), matching the
            // standalone sync_parameters RTT semantics.
            let started = quanta::Instant::now();

            let mut combined = simple_query(&set_sql);
            combined.put(&message[..]);

            server
                .send_and_flush_timeout(&combined, Duration::from_secs(5))
                .await?;

            log_client_to_server(&self.addr_str, server.get_process_id(), &combined);

            // Consume the SET response through its ReadyForQuery. Transport
            // failures still invalidate the backend; a complete SQL rejection
            // leaves the following client query intact on the same stream.
            let set_outcome = server.swallow_set_response().await?;
            server.clear_internal_set_cleanup_state();

            match set_outcome {
                SetResponseOutcome::Applied => {
                    crate::web::metrics::inc_sync_params_applied();
                    crate::web::metrics::observe_sync_params_rtt_seconds(
                        started.elapsed().as_secs_f64(),
                    );
                }
                SetResponseOutcome::Rejected { sqlstate, .. } => {
                    let (reason, action) = if sqlstate == "57014" {
                        match server.reissue_cancel_if_marked().await {
                            Some(Ok(())) => ("query_canceled", "cancel_reissued"),
                            Some(Err(err)) => {
                                warn!(
                                    "[{}@{} #c{}] failed to reissue cancel after piggybacked \
                                     SET was canceled, backend pid={}: {err}",
                                    self.username,
                                    self.pool_name,
                                    self.connection_id,
                                    server.get_process_id(),
                                );
                                ("query_canceled", "cancel_reissue_failed")
                            }
                            None => ("query_canceled", "relay"),
                        }
                    } else {
                        ("sql_error", "relay")
                    };
                    crate::web::metrics::inc_sync_params_piggyback_rejection(reason, action);
                    warn!(
                        "[{}@{} #c{}] piggybacked SET application_name was rejected with \
                         SQLSTATE {sqlstate}; relaying the following client response",
                        self.username, self.pool_name, self.connection_id,
                    );
                }
            }

            // The backend `server_parameters` mirror is ALREADY correct here:
            // swallow_set_response calls recv(.., None), whose ParameterStatus
            // handler (protocol_io.rs:609) ALWAYS updates
            // server.server_parameters regardless of the client-params arg. No
            // explicit set_param needed.

            if !set_cleanup_commands.is_empty() {
                server.track_set_cleanup_commands(set_cleanup_commands);
            }
            if !reset_cleanup_commands.is_empty() {
                server.track_reset_cleanup_commands(reset_cleanup_commands);
            }

            // Relay the CLIENT query's own response (we already sent the
            // combined frames above, so this is relay-only - NOT a fresh
            // execute_server_roundtrip which would re-send).
            self.relay_response(server).await?;
        } else {
            if !set_cleanup_commands.is_empty() {
                server.track_set_cleanup_commands(set_cleanup_commands);
            }
            if !reset_cleanup_commands.is_empty() {
                server.track_reset_cleanup_commands(reset_cleanup_commands);
            }

            self.execute_server_roundtrip(Some(message), server).await?;
        }

        if self.transaction_mode
            && self.sql_prepare_session_pinned
            && !server.cleanup_state.needs_cleanup_prepare
        {
            self.sql_prepare_session_pinned = false;
        }

        self.stats.query();
        server.stats.query(
            query_start_at.elapsed().as_micros() as u64,
            self.server_parameters.get_application_name(),
        );

        if self.complete_transaction_if_needed(server, false) {
            self.stats.idle_read();
            return Ok(TransactionAction::Break);
        }

        Ok(TransactionAction::Continue)
    }

    /// FunctionCall is a standalone fastpath round trip, outside an extended batch.
    /// ReadyForQuery decides whether transaction pooling may release the server.
    #[inline]
    async fn handle_function_call(
        &mut self,
        message: &BytesMut,
        server: &mut Server,
        query_start_at: quanta::Instant,
    ) -> Result<TransactionAction, Error> {
        server.set_async_mode(false);
        server.set_expected_responses(0);

        self.prepared.last_bound_for_top = None;

        self.execute_server_roundtrip(Some(message), server).await?;
        self.stats.query();
        server.stats.query(
            query_start_at.elapsed().as_micros() as u64,
            self.server_parameters.get_application_name(),
        );

        if self.complete_transaction_if_needed(server, false) {
            self.stats.idle_read();
            return Ok(TransactionAction::Break);
        }

        Ok(TransactionAction::Continue)
    }

    /// Synthesize the wire response for a fast-path `DISCARD ALL`:
    /// `CommandComplete("DISCARD ALL")` followed by `ReadyForQuery`. Used by
    /// the transaction-pool interception in `try_handle_without_server` to avoid a
    /// backend round-trip whose only side effect would be clearing the
    /// backend's prepared-statement cache and temp-table state - both of
    /// which transaction pooling already isolates per-client.
    ///
    /// We intentionally do NOT clear the per-client prepared cache here.
    /// The intercept does not forward `DISCARD ALL` to PostgreSQL, so the
    /// backend's prepared-statement cache and per-server-name mappings
    /// remain intact. Wiping the per-client mapping (client_name ->
    /// server_name + `Arc<Parse>`) would therefore desynchronise pg_doorman
    /// from the unchanged backend and force every subsequent reuse of a
    /// cached statement to round-trip twice: the next `Bind` would miss
    /// pg_doorman's lookup and return SQLSTATE 26000, the driver would
    /// re-Parse, and pg_doorman would re-register the statement under a
    /// fresh server name - leaving the original backend entry orphaned in
    /// the per-server LRU. asyncpg in particular hides this with its
    /// transparent auto-reprepare, but the latency cost is real.
    ///
    /// Cross-backend correctness is preserved by
    /// `ensure_prepared_statement_is_on_server` (see
    /// `client/protocol.rs::process_bind_immediate`): on every `Bind` it
    /// checks the per-server cache via `Server::has_prepared_statement` and
    /// transparently re-Parses on demand if the backend that won this txn's
    /// checkout does not already have the statement. A second safety net
    /// in `client/protocol.rs::register_parse_to_server_cache` evicts the
    /// per-client entry if the backend rejects it. So keeping the cache
    /// across an intercepted `DISCARD ALL` is both safe and the strictly
    /// cheaper path.
    ///
    /// Note: the explicit `DEALLOCATE ALL` handler in
    /// `try_handle_without_server` (see lines ~361-368) still drops the
    /// per-client cache because that path is invoked by drivers that
    /// semantically request deallocation - not the implicit cleanup the
    /// iServ contract elides here.
    async fn respond_to_simple_discard(&mut self, in_transaction: bool) -> Result<(), Error> {
        // hot path for transaction-pool workloads - every
        // client cleanup hits the iServ DISCARD ALL interception. The
        // previous shape built three `BytesMut` per call
        // (`BytesMut::new()` + `command_complete("DISCARD ALL")` +
        // `ready_for_query(in_transaction)`) just to ship 23 fixed
        // bytes that never change. The two responses are pre-computed
        // as `&'static [u8]` byte slices so the hot path is one
        // `write_all` syscall with zero heap activity.
        //
        // Byte layout:
        //   CommandComplete: 'C' + i32 len=16 BE + "DISCARD ALL" + '\0'
        //   ReadyForQuery:   'Z' + i32 len=5  BE + status ('I' or 'T')
        // The `len` field of CommandComplete = body bytes (4 length
        // bytes + payload + null), exactly matching `command_complete()`
        // in `messages/protocol.rs:598`.
        static DISCARD_ALL_RESPONSE_IDLE: &[u8] = &[
            b'C', 0, 0, 0, 16, b'D', b'I', b'S', b'C', b'A', b'R', b'D', b' ', b'A', b'L', b'L', 0,
            b'Z', 0, 0, 0, 5, b'I',
        ];
        static DISCARD_ALL_RESPONSE_IN_TX: &[u8] = &[
            b'C', 0, 0, 0, 16, b'D', b'I', b'S', b'C', b'A', b'R', b'D', b' ', b'A', b'L', b'L', 0,
            b'Z', 0, 0, 0, 5, b'T',
        ];
        let response: &[u8] = if in_transaction {
            DISCARD_ALL_RESPONSE_IN_TX
        } else {
            DISCARD_ALL_RESPONSE_IDLE
        };
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        write_all_flush_timeout(&mut self.write, response, write_timeout).await?;
        Ok(())
    }

    /// Handle Sync (S) or Flush (H) message.
    /// Returns the action to take after processing.
    #[inline]
    async fn handle_sync_flush(
        &mut self,
        message: &BytesMut,
        server: &mut Server,
        query_start_at: quanta::Instant,
        code: char,
    ) -> Result<TransactionAction, Error> {
        // Add the sync/flush message to buffer
        self.buffer.put(&message[..]);

        if code == 'H' {
            let was_async = server.is_async();
            // For Flush, enter async mode
            server.set_async_mode(true);
            // Mark this client as async client forever
            self.prepared.async_client = true;
            self.stats.set_async_client();
            debug!(
                "[{}@{} #c{}] client {} entered async mode (Flush) pid={}",
                self.username,
                self.pool_name,
                self.connection_id,
                self.addr,
                server.get_process_id()
            );

            // Calculate expected responses from batch operations BEFORE any clearing
            let mut expected_responses = Vec::with_capacity(self.prepared.batch_operations.len());
            for op in &self.prepared.batch_operations {
                match op {
                    BatchOperation::ParseSent { .. } => {
                        expected_responses.push(AsyncExpectedResponse::Operation)
                    } // ParseComplete
                    BatchOperation::Bind { .. } => {
                        expected_responses.push(AsyncExpectedResponse::Operation)
                    } // BindComplete
                    BatchOperation::Describe { .. } => {
                        expected_responses.push(AsyncExpectedResponse::Operation); // ParamDesc
                        expected_responses.push(AsyncExpectedResponse::Describe);
                        // RowDesc/NoData
                    }
                    BatchOperation::DescribePortal => {
                        expected_responses.push(AsyncExpectedResponse::Describe);
                        // RowDesc/NoData
                    }
                    BatchOperation::Execute => {
                        expected_responses.push(AsyncExpectedResponse::Operation)
                    } // CommandComplete/EmptyQuery/Suspended
                    BatchOperation::Close => {
                        expected_responses.push(AsyncExpectedResponse::Operation)
                    } // CloseComplete
                    BatchOperation::ParseSkipped { .. } => {} // No server response expected
                }
            }
            let expected = expected_responses.len() as u32;
            server.set_expected_response_sequence(expected_responses);
            debug!(
                "[{}@{} #c{}] flush: expecting {} responses from server",
                self.username, self.pool_name, self.connection_id, expected
            );

            // A fresh Flush batch that produces no backend responses must not
            // leave the checked-out backend pinned in async mode. This covers
            // cached Parse-only batches and empty Flush frames: PostgreSQL has
            // no response to send, and forwarding the bare Flush would make
            // recv() short-circuit immediately while release still refuses an
            // async backend. If we were already in async mode from an earlier
            // unsynced batch, keep the existing hold until Sync reconciles the
            // backend protocol state.
            if expected == 0 && !was_async {
                self.write_synthetic_parse_completes().await?;
                server.set_async_mode(false);
                server.set_expected_responses(0);
                self.buffer.clear();
                self.prepared.reset_batch();

                if self.complete_transaction_if_needed(server, true) {
                    return Ok(TransactionAction::Break);
                }

                return Ok(TransactionAction::Continue);
            }

            // If Flush contains only skipped Parse operations, PostgreSQL has
            // no response-producing messages to flush. Emit synthetic
            // ParseComplete immediately to avoid waiting for bytes that cannot
            // arrive. Mixed batches must go through batch reordering so
            // synthetic ParseComplete stays at its original batch position
            // relative to backend responses from earlier/later operations.
            if expected == 0 && !self.prepared.skipped_parses.is_empty() {
                self.write_synthetic_parse_completes().await?;
            }
        } else {
            // For Sync, exit async mode
            server.set_async_mode(false);
            server.set_expected_responses(0);
        }

        self.execute_server_roundtrip(None, server).await?;

        // Batch is complete — send deferred eviction Close messages.
        // These statements were evicted from the LRU during this batch but
        // kept alive on PostgreSQL so that Binds in the buffer could succeed.
        server.send_deferred_eviction_closes().await?;

        // Buffer was flushed to PostgreSQL — all deferred Parse messages
        // have reached the server. Clear the pending flag so checkin_cleanup
        // won't trigger unnecessary DEALLOCATE ALL.
        server.has_pending_cache_entries = false;

        self.stats.query();
        // /api/top/queries duration accounting. The whole batch's elapsed
        // time is attributed to the last Bind's hash; multi-Bind batches
        // give the duration to whichever Bind was last (approximation).
        let micros = query_start_at.elapsed().as_micros() as u64;
        if let Some((hash, anon)) = self.prepared.last_bound_for_top.take() {
            crate::server::record_query_duration_us(hash, anon, micros);
        }
        server
            .stats
            .query(micros, self.server_parameters.get_application_name());

        self.buffer.clear();
        if code != 'H' && !server.in_transaction() {
            self.prepared.clear_portal_cleanup_commands();
        }
        // Reset batch state for next batch
        self.prepared.reset_batch();

        if self.complete_transaction_if_needed(server, true) {
            return Ok(TransactionAction::Break);
        }

        Ok(TransactionAction::Continue)
    }

    async fn write_synthetic_parse_completes(&mut self) -> Result<(), Error> {
        let count = self.prepared.skipped_parses.len();
        if count == 0 {
            return Ok(());
        }
        debug!(
            "[{}@{} #c{}] flush: injecting {} synthetic ParseComplete for cached Parse",
            self.username, self.pool_name, self.connection_id, count
        );
        let mut synthetic_response = BytesMut::with_capacity(count * 5);
        for _ in 0..count {
            synthetic_response.extend_from_slice(&PARSE_COMPLETE_MSG);
        }
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        write_all_flush_timeout(&mut self.write, &synthetic_response, write_timeout).await?;
        self.prepared.skipped_parses.clear();
        self.prepared.batch_operations.clear();
        Ok(())
    }

    async fn flush_copy_buffer_with_timeout(&mut self, server: &mut Server) -> Result<(), Error> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        server
            .send_and_flush_timeout(&self.buffer, Duration::from_secs(5))
            .await?;

        self.buffer.clear();
        Ok(())
    }

    async fn recv_copy_completion_with_timeout(
        &mut self,
        server: &mut Server,
    ) -> Result<BytesMut, Error> {
        let deadline = tokio::time::Instant::now() + HOUSEKEEPING_TIMEOUT;
        match tokio::time::timeout_at(
            deadline,
            server.recv(&mut self.write, Some(&mut self.server_parameters)),
        )
        .await
        {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => {
                server.mark_bad(&format!("COPY FROM completion recv failed: {err}"));
                Err(err)
            }
            Err(_) => {
                server.mark_bad("COPY FROM completion recv timeout");
                Err(Error::SocketError(
                    "timeout waiting for COPY completion response".to_string(),
                ))
            }
        }
    }

    /// Handle CopyData (d) message.
    /// Returns the action to take after processing.
    #[inline]
    async fn handle_copy_data(
        &mut self,
        message: &BytesMut,
        server: &mut Server,
    ) -> Result<TransactionAction, Error> {
        self.ensure_copy_mode(server)?;

        // flush BEFORE put when this CopyData would push
        // the buffer past `BUFFER_FLUSH_THRESHOLD`. Pre-fix order put
        // unconditionally then size-gated - a single CopyData frame
        // up to PG's ~1 GiB protocol max would land in `self.buffer`
        // before the flush ever ran, allowing a per-client memory
        // amplification (attacker-chosen size up to MAX_MESSAGE_SIZE).
        // Now any frame that would exceed the threshold triggers a
        // flush of the existing buffer first, then the message is
        // forwarded directly in the next iteration's gate.
        if !self.buffer.is_empty() && self.buffer.len() + message.len() > BUFFER_FLUSH_THRESHOLD {
            self.flush_copy_buffer_with_timeout(server).await?;
        }
        self.buffer.put(&message[..]);

        // Forward immediately if this single message is already over
        // the threshold (no point buffering - drain now to bound RSS).
        if self.buffer.len() > BUFFER_FLUSH_THRESHOLD {
            self.flush_copy_buffer_with_timeout(server).await?;
        }

        Ok(TransactionAction::Continue)
    }

    /// Handle CopyDone (c) or CopyFail (f) message.
    /// Returns the action to take after processing.
    async fn handle_copy_done_fail(
        &mut self,
        message: &BytesMut,
        server: &mut Server,
    ) -> Result<TransactionAction, Error> {
        self.ensure_copy_mode(server)?;
        // We may already have some copy data in the buffer, add this message to buffer
        self.buffer.put(&message[..]);

        self.flush_copy_buffer_with_timeout(server).await?;

        // COPY FROM STDIN completion is a synchronous backend response
        // sequence (CommandComplete/ErrorResponse through ReadyForQuery).
        // If the COPY was entered via extended Flush, async mode may have
        // expected_responses == 0 after CopyInResponse; leaving it armed would
        // make Server::recv return before reading the completion frames.
        server.set_async_mode(false);
        server.set_expected_responses(0);

        let response = self.recv_copy_completion_with_timeout(server).await?;

        self.stats.active_write();
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        match write_all_flush_timeout(&mut self.write, &response, write_timeout).await {
            Ok(_) => self.stats.active_idle(),
            Err(err) => {
                server.wait_available().await;
                server.mark_bad(
                    format!(
                        "failed to flush CopyDone response to client {}: {:?}",
                        self.addr, err
                    )
                    .as_str(),
                );
                return Err(err);
            }
        };

        server.send_deferred_eviction_closes().await?;

        if self.complete_transaction_if_needed(server, false) {
            return Ok(TransactionAction::Break);
        }

        Ok(TransactionAction::Continue)
    }

    async fn write_shutdown_error_and_disconnect(&mut self) -> Result<(), Error> {
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        if let Err(err) = error_response_timeout(
            &mut self.write,
            "pooler is shut down now",
            "58006",
            write_timeout,
        )
        .await
        {
            warn!(
                "[{}@{} #c{}] timeout writing shutdown error to client {}: {err}",
                self.username, self.pool_name, self.connection_id, self.addr
            );
        }
        self.stats.disconnect();
        Ok(())
    }

    async fn write_checkout_error(&mut self, message: &str, sqlstate: &str) -> Result<(), Error> {
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        error_response_timeout(&mut self.write, message, sqlstate, write_timeout).await
    }

    /// Handle a connected and authenticated client.
    pub async fn handle(&mut self) -> Result<(), Error> {
        // The client wants to cancel a query it has issued earlier.
        if self.cancel_mode {
            return self.handle_cancel_mode().await;
        }
        self.stats.register(self.stats.clone());
        let pool = match self.admin {
            true => None,
            false => Some(self.get_pool().await?),
        };
        #[cfg(unix)]
        if let Some(pool) = pool.as_ref() {
            self.migration_pool = Some(pool.clone());
            if !self.migration_pool_is_dynamic {
                self.migration_pool_is_dynamic = crate::pool::is_dynamic_pool(&self.cached_pool_id);
            }
        }

        let mut query_start_at: quanta::Instant;
        let mut deferred_extended_begin = None;
        let mut pre_server_replay = VecDeque::new();
        #[cfg(unix)]
        let mut migration_wake_allowed = true;
        loop {
            self.stats.idle_read();

            // Try to migrate this client to the new process during graceful reload.
            // At this point: no server checked out, no pending transaction,
            // write buffer flushed from previous iteration, and no partial
            // protocol frame in the reusable read buffer.
            // Single atomic load - reused for both the deferred-log branch
            // and the actual migration branch to avoid redundant reads.
            #[cfg(unix)]
            if migration_in_progress() && !self.admin {
                if self.client_pending_begin.is_some()
                    || deferred_extended_begin.is_some()
                    || !pre_server_replay.is_empty()
                    || !self.read.buffer().is_empty()
                    || !self.read_buf.is_empty()
                {
                    debug!(
                        "[{}@{} #c{}] migration deferred: pending_begin={} extended_begin={} replay={} read_buf={} partial_msg={}",
                        self.username,
                        self.pool_name,
                        self.connection_id,
                        self.client_pending_begin.is_some(),
                        deferred_extended_begin.is_some(),
                        pre_server_replay.len(),
                        self.read.buffer().len(),
                        self.read_buf.len()
                    );
                } else {
                    match MIGRATION_TX.get() {
                        None => {
                            warn!(
                                "[{}@{} #c{}] migration channel not ready",
                                self.username, self.pool_name, self.connection_id
                            );
                            migration_wake_allowed = false;
                            if SHUTDOWN_IN_PROGRESS.load(Ordering::Relaxed) {
                                warn!(
                                    "[{}@{} #c{}] dropping unmigrated client {}: shutting down",
                                    self.username, self.pool_name, self.connection_id, self.addr
                                );
                                self.write_shutdown_error_and_disconnect().await?;
                                return Ok(());
                            }
                        }
                        Some(tx) => {
                            // Reserve a migration channel slot, waiting up to the
                            // published migration deadline for the receiver to
                            // drain a slot - instead of dropping the client on
                            // the first full-channel poll. The channel is
                            // intentionally small (heap-bounded, often 6), so
                            // with N >> capacity simultaneously-idle clients a
                            // non-blocking `try_reserve` lost most of them.
                            // `reserve().await` does NOT duplicate the client fd:
                            // `prepare_migration` dups the socket only AFTER a
                            // permit is granted, so the EMFILE-safety invariant
                            // of the old path is preserved while clients pipeline
                            // through the channel as the receiver drains it. The
                            // absolute deadline (<= shutdown_timeout) guarantees
                            // an unreachable/stuck successor (whose receiver never
                            // drains, so no slot ever frees and the channel is
                            // never closed) can never hang a client past the
                            // parent's shutdown window.
                            let deadline =
                                crate::app::server::migration_deadline().unwrap_or_else(|| {
                                    tokio::time::Instant::now() + Duration::from_secs(10)
                                });
                            let drop_reason: Option<&'static str> = match tokio::time::timeout_at(
                                deadline,
                                tx.reserve(),
                            )
                            .await
                            {
                                Ok(Ok(permit)) => match self.prepare_migration() {
                                    Ok(payload) => {
                                        permit.send(payload);
                                        info!(
                                                "[{}@{} #c{}] client {} queued for migration to new process",
                                                self.username,
                                                self.pool_name,
                                                self.connection_id,
                                                self.addr
                                            );
                                        // Note: do NOT decrement CURRENT_CLIENT_COUNT here.
                                        // The caller (server.rs accept loop) decrements it
                                        // unconditionally after client_entrypoint() returns.
                                        // The payload is enqueued, not yet delivered: actual
                                        // socket handoff happens in migration_sender_task.
                                        return Ok(());
                                    }
                                    Err(e) => {
                                        warn!(
                                            "[{}@{} #c{}] prepare_migration failed: {e}",
                                            self.username, self.pool_name, self.connection_id
                                        );
                                        // Permit dropped here: the reserved slot is
                                        // released back to the channel for another client.
                                        Some("prepare_failed")
                                    }
                                },
                                Ok(Err(_closed)) => {
                                    warn!(
                                        "[{}@{} #c{}] migration channel closed before reservation",
                                        self.username, self.pool_name, self.connection_id
                                    );
                                    Some("channel_closed")
                                }
                                Err(_elapsed) => {
                                    warn!(
                                            "[{}@{} #c{}] migration reservation timed out before deadline",
                                            self.username, self.pool_name, self.connection_id
                                        );
                                    Some("deadline")
                                }
                            };
                            if let Some(reason) = drop_reason {
                                // Stop asking to migrate; either keep talking to
                                // the old process (migration in progress but not
                                // yet shutting down) or drop the session (the old
                                // process is shutting down and cannot serve it).
                                migration_wake_allowed = false;
                                if SHUTDOWN_IN_PROGRESS.load(Ordering::Relaxed) {
                                    warn!(
                                        "[{}@{} #c{}] dropping unmigrated client {}: shutting down ({reason})",
                                        self.username, self.pool_name, self.connection_id, self.addr
                                    );
                                    crate::web::metrics::record_migration_client_dropped(reason);
                                    self.write_shutdown_error_and_disconnect().await?;
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }

            #[cfg(unix)]
            let (message, replayed_pre_server_message) =
                if let Some(message) = pre_server_replay.pop_front() {
                    (message, true)
                } else {
                    let migration_wake_enabled = migration_wake_allowed
                        && self.client_pending_begin.is_none()
                        && deferred_extended_begin.is_none();
                    match read_idle_message_or_migration_notice(
                        &mut self.read,
                        &mut self.read_buf,
                        self.max_memory_usage,
                        self.admin,
                        migration_wake_enabled,
                    )
                    .await
                    {
                        Ok(IdleClientRead::Message(message)) => (message, false),
                        Ok(IdleClientRead::MigrationRequested) => continue,
                        Err(err) => return self.process_error(err).await,
                    }
                };
            #[cfg(not(unix))]
            let (message, replayed_pre_server_message) = if let Some(message) =
                pre_server_replay.pop_front()
            {
                (message, true)
            } else {
                match read_message_reuse(&mut self.read, &mut self.read_buf, self.max_memory_usage)
                    .await
                {
                    Ok(message) => (message, false),
                    Err(err) => return self.process_error(err).await,
                }
            };
            if message[0] as char == 'X' {
                debug!(
                    "[{}@{} #c{}] client {} sent Terminate",
                    self.username, self.pool_name, self.connection_id, self.addr
                );
                self.stats.disconnect();
                return Ok(());
            }

            if SHUTDOWN_IN_PROGRESS.load(Ordering::Relaxed)
                && !migration_in_progress()
                && !self.admin
            {
                warn!(
                    "[{}@{} #c{}] dropping client {}: shutting down",
                    self.username, self.pool_name, self.connection_id, self.addr
                );
                self.write_shutdown_error_and_disconnect().await?;
                return Ok(());
            }
            // Handle admin database queries.
            if self.admin {
                handle_admin(&mut self.write, message, self.client_server_map.clone())
                    .await
                    .inspect_err(|_| self.stats.disconnect())?;
                continue;
            }

            let message = if replayed_pre_server_message {
                message
            } else {
                match self
                    .try_handle_deferred_extended_begin(
                        message,
                        &mut deferred_extended_begin,
                        &mut pre_server_replay,
                    )
                    .await?
                {
                    PreServerMessageAction::Handled => continue,
                    PreServerMessageAction::Process(message) => message,
                }
            };

            query_start_at = now();
            let current_pool = pool.as_ref().unwrap();

            // Handle serverless fast paths before backend checkout.
            if self.client_pending_begin.is_none()
                && self
                    .try_handle_without_server(&message, current_pool, query_start_at)
                    .await?
            {
                continue;
            }

            // Micro-optimization: if first message is standalone BEGIN,
            // synthesize response and defer actual BEGIN to next query.
            // BEGIN itself doesn't perform any server operations, it only
            // reserves a connection which is wasteful if client is slow.
            if is_standalone_begin(&message) && self.client_pending_begin.is_none() {
                debug!(
                    "[{}@{} #c{}] deferring BEGIN for client {}",
                    self.username, self.pool_name, self.connection_id, self.addr
                );

                // Send synthetic response: CommandComplete('BEGIN') + ReadyForQuery('T')
                // CommandComplete: 'C' + len(10) + "BEGIN\0"
                // ReadyForQuery: 'Z' + len(5) + 'T' (in transaction)
                const SYNTHETIC_BEGIN_RESPONSE: &[u8] = &[
                    b'C', 0, 0, 0, 10, b'B', b'E', b'G', b'I', b'N', 0, // CommandComplete
                    b'Z', 0, 0, 0, 5, b'T', // ReadyForQuery('T')
                ];
                let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
                write_all_flush_timeout(&mut self.write, SYNTHETIC_BEGIN_RESPONSE, write_timeout)
                    .await?;

                // Store pending BEGIN for next query
                self.client_pending_begin = Some(message);
                continue; // Return to main loop, wait for next message
            }

            // Check if we have a pending BEGIN to send with this query
            let pending_begin = self.client_pending_begin.take();

            let shutdown_in_progress = {
                // start server.
                // Grab a server from the pool.
                let connecting_at = now();
                self.stats.waiting();
                let mut conn = loop {
                    match current_pool.database.get().await {
                        Ok(mut conn) => {
                            // shard-local atomic consume of the
                            // cancel-quarantine marker; only one client wins
                            // under concurrent races.
                            // a FRESH marker (a cancel for this backend may
                            // still be in flight) evicts the backend so the late
                            // cancel cannot hit this client's query; a STALE
                            // marker means PG recycled this OS pid to a healthy
                            // NEW backend, so reuse it instead of needlessly
                            // evicting (the entry is cleared either way).
                            match canceled_pids_consume(conn.get_process_id()) {
                                CancelMarker::Fresh => {
                                    conn.mark_bad("connection was cancel-quarantined");
                                    continue; // try to find another server.
                                }
                                CancelMarker::Stale | CancelMarker::Absent => {}
                            }
                            // checkin_cleanup before give server to client.
                            match conn.checkin_cleanup().await {
                                Ok(()) => break conn,
                                Err(err) => {
                                    warn!(
                                        "[{}@{} #c{}] server cleanup error: {err}",
                                        self.username, self.pool_name, self.connection_id,
                                    );
                                    continue;
                                }
                            };
                        }
                        Err(err) => {
                            // Client is attempting to get results from the server,
                            // but we were unable to grab a connection from the pool
                            // We'll send back an error message and clean the extended
                            // protocol buffer
                            self.stats.idle_read();
                            // Mirrors the SQLSTATE in the ErrorResponse below
                            // so the per-pool breakdown reflects checkout
                            // failures alongside PG-side errors.
                            //
                            // Special case: PG itself rejected the
                            // operator-supplied startup_parameters cascade.
                            // Forward the verbatim sqlstate/message so the
                            // client receives the same PG-native error it
                            // would have seen connecting to PG directly,
                            // instead of the generic 53300
                            // (too_many_connections) checkout-fallback.
                            // Same shape as the rest of the branch (reset
                            // buffered state on 'S', error_response, log,
                            // return), only the SQLSTATE and message differ.
                            if let crate::pool::PoolError::Backend(
                                Error::ConnectResourceExhausted(msg),
                            ) = &err
                            {
                                current_pool.address.stats.error_with_sqlstate("53000");
                                self.stats.checkout_error();
                                // surface the error in per-client SHOW
                                // CLIENTS / /api/clients.
                                self.stats.error();

                                if message[0] as char == 'S' {
                                    self.reset_buffered_state();
                                }

                                self.write_checkout_error(
                                    checkout_resource_exhausted_client_message(msg),
                                    "53000",
                                )
                                .await?;

                                error!(
                                    "[{}@{} #c{}] local resource exhausted while getting server connection: {err}",
                                    self.username, self.pool_name, self.connection_id,
                                );
                                return Err(Error::AllServersDown);
                            }

                            if let crate::pool::PoolError::Backend(
                                Error::ServerStartupParameterRejection {
                                    sqlstate,
                                    message: pg_message,
                                    ..
                                },
                            ) = &err
                            {
                                current_pool.address.stats.error_with_sqlstate(sqlstate);
                                self.stats.checkout_error();
                                self.stats.error();

                                if message[0] as char == 'S' {
                                    self.reset_buffered_state();
                                }

                                self.write_checkout_error(pg_message, sqlstate).await?;

                                error!(
                                    "[{}@{} #c{}] PG rejected startup_parameters: sqlstate={} {}",
                                    self.username,
                                    self.pool_name,
                                    self.connection_id,
                                    sqlstate,
                                    pg_message,
                                );
                                return Err(Error::AllServersDown);
                            }

                            current_pool.address.stats.error_with_sqlstate("53300");
                            self.stats.checkout_error();
                            self.stats.error();

                            if message[0] as char == 'S' {
                                self.reset_buffered_state();
                            }

                            // client-visible message is
                            // partitioned by PoolError category. Safe
                            // variants (Timeout, Closed, Exhausted) have
                            // benign Display strings (no host addresses,
                            // no Patroni URLs); they're safe to include
                            // so existing dashboards / tests that grep
                            // for "timeout" still work. Unsafe variants
                            // (Backend with SocketError/ConnectError)
                            // have Display strings that leak topology;
                            // they collapse to a generic message and the
                            // full detail goes to `error!` log only.
                            let safe_suffix = matches!(
                                err,
                                crate::pool::PoolError::Timeout(_)
                                    | crate::pool::PoolError::Closed
                                    | crate::pool::PoolError::DbLimitExhausted(_)
                            );
                            let client_msg = if safe_suffix {
                                format!(
                                    "Could not get a database connection from the pool: {err}. Please try again later."
                                )
                            } else {
                                "Could not get a database connection from the pool. All servers may be busy or down. Please try again later.".to_string()
                            };
                            self.write_checkout_error(&client_msg, "53300").await?;

                            error!(
                                "[{}@{} #c{}] failed to get server connection: {err}",
                                self.username, self.pool_name, self.connection_id,
                            );
                            return Err(Error::AllServersDown);
                        }
                    };
                };
                let server = conn.deref_mut();
                server.stats.active(self.stats.application_name());
                let checkout_us = connecting_at.elapsed().as_micros() as u64;
                server
                    .stats
                    .checkout_time(checkout_us, self.stats.application_name());
                // Update client-side wait tracking so SHOW POOLS maxwait
                // reflects real checkout peaks, not the zero from init.
                self.stats
                    .total_wait_time
                    .fetch_add(checkout_us, Ordering::Relaxed);
                self.stats
                    .max_wait_time
                    .fetch_max(checkout_us, Ordering::Relaxed);
                if checkout_us >= 500_000 {
                    let status = current_pool.database.status();
                    let scaling = current_pool.database.scaling_stats();
                    warn!(
                        "[{}@{} #c{}] slow checkout: {}ms pid={} size={}/{} avail={} waiting={} inflight={} creates={} gate_waits={} bg_timeout={} antic_ok={} antic_to={} fallback={}",
                        self.username,
                        self.pool_name,
                        self.connection_id,
                        checkout_us / 1_000,
                        server.get_process_id(),
                        status.size, status.max_size,
                        status.available,
                        status.waiting,
                        scaling.inflight_creates,
                        scaling.creates_started,
                        scaling.burst_gate_waits,
                        scaling.burst_gate_budget_exhausted,
                        scaling.anticipation_wakes_notify,
                        scaling.anticipation_wakes_timeout,
                        scaling.create_fallback,
                    );
                }
                let server_active_at = now();

                // Server is assigned to the client in case the client wants to
                // cancel a query later.
                server.claim(self.connection_id as i32, self.secret_key);
                self.connected_to_server = true;

                // RAII guard: increments CLIENTS_IN_TRANSACTIONS now,
                // decrements automatically when this block exits (normal or early return).
                let _tx_guard = TransactionGuard::new();

                // Update statistics
                self.stats.active_idle();
                self.last_server_stats = Some(server.stats.clone());

                debug!(
                    "[{}@{} #c{}] client {} acquired server pid={}",
                    self.username,
                    self.pool_name,
                    self.connection_id,
                    self.addr,
                    server.get_process_id()
                );

                if current_pool.settings.sync_server_parameters {
                    // classify the parameter diff once at checkout,
                    // then dispatch:
                    //   - Empty       -> nothing to sync (skip counter), same
                    //                    no-op as before.
                    //   - AppNameOnly -> DEFER: store the ready-to-send SET SQL
                    //                    on the client and piggyback it onto the
                    //                    first simple-query ('Q') message (see
                    //                    handle_simple_query), or flush it
                    //                    standalone before any other
                    //                    backend-bound first message (the deferred-SET
                    //                    guard below). This removes the
                    //                    standalone SET round-trip on the
                    //                    common app_name-only checkout.
                    //   - Complex     -> standalone round-trip using the diff
                    //                    already computed by the classifier.
                    // after server.claim()
                    // the cancel-routing entry is live; every prep error path
                    // must release_after_inner_handler_error() before
                    // propagating, or the (connection_id, secret_key) row is
                    // orphaned until Client::Drop - a window where a parallel
                    // CancelRequest could fire at a recycled backend pid.
                    let sync_plan = match server.compute_sync_plan(&self.server_parameters) {
                        Ok(plan) => plan,
                        Err(err) => {
                            self.release_after_inner_handler_error();
                            return Err(err);
                        }
                    };
                    match sync_plan {
                        SyncPlan::Empty => {
                            crate::web::metrics::inc_sync_params_plan("empty", "none");
                            crate::web::metrics::inc_sync_params_skipped();
                        }
                        SyncPlan::AppNameOnly(sql) => {
                            // Single consumer per checkout; take()n on the
                            // first client message (success AND error paths).
                            self.pending_app_name_set = Some(sql);
                        }
                        SyncPlan::Complex(parameter_diff) => {
                            crate::web::metrics::inc_sync_params_plan("complex", "standalone");
                            if let Err(err) = server.sync_parameter_diff(parameter_diff).await {
                                self.release_after_inner_handler_error();
                                return Err(err);
                            }
                        }
                    }
                }
                server.set_async_mode(false);

                // If we deferred BEGIN, send it to server first (without forwarding response to client)
                // Client already received synthetic response, so we discard the real server response
                if let Some(begin_msg) = pending_begin {
                    // A deferred `SET application_name` MUST be
                    // flushed BEFORE the deferred BEGIN, not piggybacked onto the
                    // first query inside the transaction. `application_name` is a
                    // non-LOCAL GUC, so a `SET` issued inside a transaction the
                    // client later ROLLBACKs is reverted by PostgreSQL - leaving
                    // the reused backend advertising the PREVIOUS service's
                    // application_name in pg_stat_activity (audit mis-attribution)
                    // until the next checkout re-syncs it. Baseline
                    // `sync_parameters` ran the SET before the BEGIN (outside any
                    // transaction); restore that ordering here. Flushed standalone
                    // via the same `small_simple_query` the deferred-SET guard uses, and
                    // `take()`n so the 'Q' piggyback / deferred-SET consumers then see
                    // `None` (no double-SET). This costs exactly the one SET
                    // round-trip baseline already paid on this path - not a new
                    // regression.
                    if let Some(set_sql) = self.pending_app_name_set.take() {
                        crate::web::metrics::inc_sync_params_plan(
                            "app_name_only",
                            "deferred_begin_preflush",
                        );
                        let started = quanta::Instant::now();
                        if let Err(err) = server.small_simple_query(&set_sql).await {
                            self.release_after_inner_handler_error();
                            return Err(err);
                        }
                        server.clear_internal_set_cleanup_state();
                        crate::web::metrics::inc_sync_params_applied();
                        crate::web::metrics::observe_sync_params_rtt_seconds(
                            started.elapsed().as_secs_f64(),
                        );
                    }

                    debug!(
                        "[{}@{} #c{}] sending deferred BEGIN to server pid={}",
                        self.username,
                        self.pool_name,
                        self.connection_id,
                        server.get_process_id()
                    );

                    // Send BEGIN to server
                    let deadline = tokio::time::Instant::now() + HOUSEKEEPING_TIMEOUT;
                    server.begin_internal_round_trip();
                    match tokio::time::timeout_at(deadline, server.send_and_flush(&begin_msg)).await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            self.release_after_inner_handler_error();
                            return Err(err);
                        }
                        Err(_) => {
                            server.mark_bad("deferred BEGIN send timeout");
                            self.release_after_inner_handler_error();
                            let _ = server;
                            drop(conn);
                            let write_timeout =
                                config_arc().general.proxy_copy_data_timeout.as_std();
                            let _ = error_response_timeout(
                                &mut self.write,
                                "pooler is shut down now (deferred BEGIN timeout: server did not finish within the timeout period)",
                                "58006",
                                write_timeout,
                            )
                            .await;
                            return Err(Error::SocketError(
                                "timeout sending deferred BEGIN".to_string(),
                            ));
                        }
                    }

                    // Receive and discard response (client already got synthetic response)
                    // Using sink() to avoid forwarding to client
                    loop {
                        match tokio::time::timeout_at(
                            deadline,
                            server.recv(&mut tokio::io::sink(), Some(&mut self.server_parameters)),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {
                                if !server.is_data_available() {
                                    break;
                                }
                            }
                            Ok(Err(err)) => {
                                server.mark_bad(&format!("deferred BEGIN failed: {err}"));
                                self.release_after_inner_handler_error();
                                return Err(err);
                            }
                            Err(_) => {
                                server.mark_bad("deferred BEGIN recv timeout");
                                self.release_after_inner_handler_error();
                                let _ = server;
                                drop(conn);
                                let write_timeout =
                                    config_arc().general.proxy_copy_data_timeout.as_std();
                                let _ = error_response_timeout(
                                    &mut self.write,
                                    "pooler is shut down now (deferred BEGIN timeout: server did not finish within the timeout period)",
                                    "58006",
                                    write_timeout,
                                )
                                .await;
                                return Err(Error::SocketError(
                                    "timeout waiting for deferred BEGIN response".to_string(),
                                ));
                            }
                        }
                    }
                    server.finish_internal_round_trip();

                    // Reset query_start_at for the actual query
                    query_start_at = now();
                }

                let mut initial_message = Some(message);

                // Transaction loop. Multiple queries can be issued by the client here.
                // The connection belongs to the client until the transaction is over,
                // or until the client disconnects if we are in session mode.
                //
                // If the client is in session mode, no more custom protocol
                // commands will be accepted.
                loop {
                    let message = match take_queued_pre_server_message(
                        &mut initial_message,
                        &mut pre_server_replay,
                    ) {
                        None => {
                            self.stats.active_read();
                            match self.wait_for_next_message(server).await {
                                Ok(NextClientMessage::Message(msg)) => msg,
                                Ok(NextClientMessage::ServerDead) => {
                                    warn!(
                                        "[{}@{} #c{}] server died while idle in transaction pid={}",
                                        self.username,
                                        self.pool_name,
                                        self.connection_id,
                                        server.get_process_id()
                                    );
                                    server
                                        .mark_bad("server closed while client idle in transaction");
                                    self.stats.disconnect();
                                    self.connected_to_server = false;
                                    self.release();
                                    drop(conn);
                                    let write_timeout =
                                        config_arc().general.proxy_copy_data_timeout.as_std();
                                    let _ = error_response_timeout(
                                        &mut self.write,
                                        "server closed the connection unexpectedly while client was idle in transaction",
                                        "08006",
                                        write_timeout,
                                    )
                                    .await;
                                    return Ok(());
                                }
                                Err(err) => {
                                    self.stats.disconnect();
                                    self.connected_to_server = false;
                                    // run cleanup but ALWAYS release(),
                                    // even when finalize_checkin returns Err.
                                    // The `?` short-circuit used to leave the
                                    // (connection_id, secret_key) entry in
                                    // client_server_map until Client::Drop,
                                    // creating a window where an in-flight
                                    // cancel could fire at whichever backend
                                    // pid now lived behind that map row.
                                    let finalize_res = server.finalize_checkin().await;
                                    self.release();
                                    finalize_res?;
                                    return self.process_error(err).await;
                                }
                            }
                        }

                        Some(message) => message,
                    };
                    self.stats.active_idle();

                    // Session mode: reset query timer per message so query_time
                    // reflects individual queries, not cumulative session duration.
                    if !self.transaction_mode {
                        query_start_at = now();
                    }

                    // The message will be forwarded to the server intact. We still would like to
                    // parse it below to figure out what to do with it.

                    // Safe to unwrap because we know this message has a certain length and has the code
                    // This reads the first byte without advancing the internal pointer and mutating the bytes
                    let code = *message.first().unwrap() as char;

                    // Deferred-SET preflush: a non-simple-query first message after checkout
                    // can't piggyback the deferred SET application_name (1a
                    // covers 'Q' only - see handle_simple_query). Flush it
                    // standalone before processing so app_name accuracy is
                    // preserved on every backend-bound path.
                    // 'X' (Terminate) needs no SET - the session is ending.
                    // take() is the single consumer here; the 'Q' arm's own
                    // take() then sees None, so there is no double-SET.
                    if code != 'Q' && code != 'X' {
                        if let Some(set_sql) = self.pending_app_name_set.take() {
                            crate::web::metrics::inc_sync_params_plan(
                                "app_name_only",
                                "non_simple_preflush",
                            );
                            let started = quanta::Instant::now();
                            if let Err(err) = server.small_simple_query(&set_sql).await {
                                self.release_after_inner_handler_error();
                                return Err(err);
                            }
                            server.clear_internal_set_cleanup_state();
                            crate::web::metrics::inc_sync_params_applied();
                            crate::web::metrics::observe_sync_params_rtt_seconds(
                                started.elapsed().as_secs_f64(),
                            );
                            // Backend mirror is already updated by recv's
                            // ParameterStatus handler; no explicit
                            // `set_param` is needed here.
                        }
                    }

                    // capture inner-handler Result so we can
                    // mark the backend bad on ANY error BEFORE
                    // propagating via `?`. Without this, errors from
                    // Parse/Bind/Describe/Execute/Close/Sync paths
                    // bubble up past the outer-loop `finalize_checkin`
                    // and the Object::Drop returns a server with
                    // unknown / possibly-corrupted protocol state to
                    // the idle pool - `checkin_cleanup` runs but
                    // release_query is skipped (the iServ contract:
                    // `pgv_free` + `pg_advisory_unlock_all` per pool).
                    // Marking bad ensures Object::Drop evicts the
                    // backend and the next checkout gets a fresh one
                    // whose release_query semantics still apply.
                    let action_result: Result<TransactionAction, Error> = match code {
                        // Query
                        'Q' => {
                            if !non_extended_protocol_can_forward(
                                self.buffer.len(),
                                self.prepared.batch_operations.len(),
                                self.prepared.skipped_parses.len(),
                            ) {
                                error!(
                                    "[{}@{} #c{}] SimpleQuery 'Q' with pending extended-protocol \
                                     state (buffer={} bytes, batch_ops={}, skipped_parses={}) - \
                                     disconnecting to avoid synthetic-response desync",
                                    self.username,
                                    self.pool_name,
                                    self.connection_id,
                                    self.buffer.len(),
                                    self.prepared.batch_operations.len(),
                                    self.prepared.skipped_parses.len()
                                );
                                Err(Error::ProtocolSyncError(
                                    "SimpleQuery 'Q' received with un-synced extended-protocol \
                                     state pending"
                                        .to_string(),
                                ))
                            } else {
                                self.handle_simple_query(&message, server, query_start_at)
                                    .await
                            }
                        }

                        // Terminate
                        'X' => {
                            // ALWAYS release(), even if finalize_checkin
                            // returns Err. `?` short-circuiting on a release-
                            // query failure used to leave the cancel-map
                            // entry alive until Client::Drop ran - during
                            // that window an in-flight cancel could fire at
                            // an unrelated backend that now owned the same
                            // (connection_id, secret_key) tuple.
                            let finalize_res = server.finalize_checkin().await;
                            self.stats.disconnect();
                            self.connected_to_server = false;
                            self.release();
                            finalize_res?;
                            return Ok(());
                        }

                        // Parse
                        'P' => self
                            .process_parse_immediate(message, current_pool, server)
                            .await
                            .map(|_| TransactionAction::Continue),

                        // Bind
                        'B' => self
                            .process_bind_immediate(message, current_pool, server)
                            .await
                            .map(|_| TransactionAction::Continue),

                        // Describe
                        // Command a client can issue to describe a earlier prepared named statement.
                        'D' => self
                            .process_describe_immediate(message, current_pool, server)
                            .await
                            .map(|_| TransactionAction::Continue),

                        // Execute
                        // Execute a prepared statement prepared in `P` and bound in `B`.
                        'E' => {
                            // cap pending extended-protocol buffer.
                            match enforce_extended_batch_buffer_cap(
                                self.buffer.len(),
                                message.len(),
                                "Execute",
                            ) {
                                Ok(()) => {
                                    self.track_execute_cleanup_attribution(server, &message);
                                    self.buffer.put(&message[..]);
                                    // Track Execute for correct ParseComplete insertion position
                                    self.prepared.batch_operations.push(BatchOperation::Execute);
                                    Ok(TransactionAction::Continue)
                                }
                                Err(e) => Err(e),
                            }
                        }

                        // Close
                        // Close the prepared statement.
                        'C' => self
                            .process_close_immediate(message)
                            .map(|_| TransactionAction::Continue),

                        // Sync or Flush
                        // Frontend (client) is asking for the query result now.
                        'S' | 'H' => {
                            self.handle_sync_flush(&message, server, query_start_at, code)
                                .await
                        }

                        // CopyData
                        'd' => self.handle_copy_data(&message, server).await,

                        // CopyDone or CopyFail
                        // Copy is done, successfully or not.
                        'c' | 'f' => self.handle_copy_done_fail(&message, server).await,

                        // FunctionCall - legacy PG protocol frame still used by
                        // libpq's `PQfn`, which underpins the entire large-object
                        // API (`lo_creat`, `lo_open`, `lo_read`, `lo_write`,
                        // `lo_close`, `lo_unlink`). Without this arm pg_doorman
                        // dropped the frame as a protocol error and libpq
                        // surfaced `server closed the connection unexpectedly`,
                        // breaking psycopg2 `lobject()`, PHP `pg_lo_*`,
                        // Perl DBD::Pg `lo_*`, and `pg_dump --large-objects`.
                        // Forward the bytes through a single server round-trip
                        // - backend replies with FunctionCallResponse('V')
                        // followed by ReadyForQuery('Z'), exactly the same
                        // termination as a SimpleQuery.
                        'F' => {
                            // fail-closed if the client pipelined
                            // Parse/Bind (buffered, not yet round-tripped)
                            // and then sent 'F' before Sync. Forwarding 'F'
                            // now would strand those buffered bytes and
                            // desync the next exchange. The legit libpq
                            // lo_* path sends a standalone 'F' with an
                            // empty buffer, so this only rejects the
                            // illegitimate pipelined sequence. Evaluate to
                            // Err (do NOT early-return) so the outer
                            // action_result handler maps ProtocolSyncError
                            // to server mark_bad + client disconnect
                            // (08P01) - the backend's protocol state is
                            // unknown once we refuse the staged batch.
                            if !non_extended_protocol_can_forward(
                                self.buffer.len(),
                                self.prepared.batch_operations.len(),
                                self.prepared.skipped_parses.len(),
                            ) {
                                error!(
                                    "[{}@{} #c{}] FunctionCall 'F' with pending extended-protocol \
                                     state (buffer={} bytes, batch_ops={}, skipped_parses={}) - \
                                     disconnecting to avoid desync",
                                    self.username,
                                    self.pool_name,
                                    self.connection_id,
                                    self.buffer.len(),
                                    self.prepared.batch_operations.len(),
                                    self.prepared.skipped_parses.len()
                                );
                                Err(Error::ProtocolSyncError(
                                    "FunctionCall 'F' received with un-synced extended-protocol \
                                     state pending"
                                        .to_string(),
                                ))
                            } else {
                                self.handle_function_call(&message, server, query_start_at)
                                    .await
                            }
                        }

                        // Some unexpected message. We either did not implement the protocol correctly
                        // or this is not a Postgres client we're talking to.
                        //
                        // earlier this branch silently
                        // `Continue`d. Any bytes already staged in
                        // `self.buffer` (Parse/Bind) would then be
                        // sent on the NEXT Sync with the unknown
                        // frame's bytes already consumed - silent
                        // driver-side desync. Disconnect the client
                        // with SQLSTATE 08P01 (protocol_violation)
                        // and propagate Err so the outer match
                        // mark_bad's the backend (any buffered
                        // extended-protocol state is unknown).
                        _ => {
                            error!(
                                "[{}@{} #c{}] unexpected message code '{}' (ASCII: {}) from client {} - disconnecting",
                                self.username, self.pool_name, self.connection_id, code, code as u8, self.addr
                            );
                            Err(Error::ProtocolSyncError(format!(
                                "unsupported client message code '{}' (ASCII {})",
                                code, code as u8
                            )))
                        }
                    };

                    let action = match action_result {
                        Ok(a) => a,
                        Err(err) => {
                            // Only mark the backend bad when the error
                            // strongly implies the backend's protocol
                            // state is unknown. A `SocketError` raised
                            // while writing the response to the CLIENT
                            // (TCP abort, client disappeared mid-row)
                            // does NOT corrupt the backend - the
                            // backend already produced a complete
                            // response and is back at ReadyForQuery.
                            // Marking it bad evicted healthy backends
                            // on every client-abort, regressing the
                            // `buffer-cleanup` scenario where rapid
                            // mid-response TCP aborts drained the
                            // entire pool.
                            //
                            // Errors arising from genuine backend
                            // mis-state are already marked bad inside
                            // `execute_server_roundtrip` /
                            // `handle_simple_query` /
                            // `process_*_immediate`, so we only need
                            // to handle the residual cases where the
                            // inner handler returned a protocol-level
                            // error without touching `mark_bad` yet.
                            if !server.is_bad() {
                                // extend the change
                                // refinement - a `SocketError`
                                // surfaced while the backend is mid-
                                // COPY or in async-pipeline mode
                                // means the backend's protocol state
                                // is unknown (it cannot trivially
                                // recover from a torn read). Without
                                // this, Object::drop returned the
                                // broken backend to the pool and the
                                // next client wasted one checkout
                                // cycle before checkin_cleanup
                                // noticed `in_copy_mode=true` and
                                // marked it bad.
                                let dirty_state = server.in_copy_mode() || server.is_async();
                                let needs_eviction = matches!(
                                    err,
                                    Error::PreparedStatementError
                                        | Error::ProtocolSyncError(_)
                                        | Error::BadQuery(_)
                                        | Error::FlushTimeout
                                ) || (matches!(err, Error::SocketError(_))
                                    && dirty_state);
                                if needs_eviction {
                                    server.mark_bad(&format!(
                                        "inner-handler protocol error for code '{code}': {err}"
                                    ));
                                }
                            }
                            let client_timeout_error = backend_timeout_client_error(&err);
                            self.release_after_inner_handler_error();
                            if let Some((message, code)) = client_timeout_error {
                                let _ = server;
                                drop(conn);
                                let write_timeout =
                                    config_arc().general.proxy_copy_data_timeout.as_std();
                                let _ = error_response_timeout(
                                    &mut self.write,
                                    message,
                                    code,
                                    write_timeout,
                                )
                                .await;
                                return Err(err);
                            }
                            return Err(err);
                        }
                    };

                    // Handle the action returned by message processor
                    match action {
                        TransactionAction::Continue => {}
                        TransactionAction::Break => break,
                    }
                }
                // Once the client query reached ReadyForQuery, cancellation must
                // no longer target this backend: the next exchange is internal
                // check-in cleanup.
                let shutdown_in_progress = SHUTDOWN_IN_PROGRESS.load(Ordering::Relaxed);
                self.connected_to_server = false;
                self.release();

                let has_buffered_response = !self.client_last_messages_in_tx.is_empty();
                if has_buffered_response {
                    self.stats.idle_write();
                }
                let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
                let buffered_response = &self.client_last_messages_in_tx;
                let client_write = &mut self.write;

                // Client delivery and backend cleanup are independent after
                // ReadyForQuery and use different sockets. Running them together
                // keeps release-query latency out of the client-visible result.
                let flush_response = async {
                    if has_buffered_response {
                        write_all_flush_timeout(client_write, buffered_response, write_timeout)
                            .await
                    } else {
                        Ok(())
                    }
                };
                let finalize_backend = async {
                    if shutdown_in_progress {
                        server.mark_bad("graceful shutdown - releasing server connection");
                        Ok(())
                    } else if !server.is_async() {
                        server.finalize_checkin().await
                    } else {
                        Ok(())
                    }
                };
                let (flush_res, finalize_res) = tokio::join!(flush_response, finalize_backend);

                if self.transaction_mode {
                    server
                        .stats
                        .add_xact_time_and_idle(server_active_at.elapsed().as_micros() as u64);
                }

                if let Err(err) = finalize_res {
                    if !server.is_bad() {
                        server.mark_bad(&format!("check-in cleanup failed: {err}"));
                    }
                    warn!(
                        "[{}@{} #c{}] check-in cleanup failed for backend pid={}: {err}",
                        self.username,
                        self.pool_name,
                        self.connection_id,
                        server.get_process_id(),
                    );
                }
                server.stats.wait_idle();

                match flush_res {
                    Ok(()) => {
                        if has_buffered_response {
                            self.client_last_messages_in_tx.clear();
                        }
                    }
                    Err(err) => {
                        warn!(
                            "[{}@{} #c{}] buffered transaction response flush to client failed: {err}",
                            self.username, self.pool_name, self.connection_id
                        );
                        self.stats.disconnect();
                        return Err(err);
                    }
                }

                shutdown_in_progress
            }; // release server.

            // TransactionGuard dropped at end of block above, counter already decremented.

            // If shutdown is in progress and migration is not available,
            // send error to client and exit. When migration is active,
            // let the client return to idle loop where it will migrate.
            if shutdown_in_progress && !migration_in_progress() {
                let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
                error_response_timeout(
                    &mut self.write,
                    "pooler is shut down now",
                    "58006",
                    write_timeout,
                )
                .await?;
                self.stats.disconnect();
                return Ok(());
            }

            self.stats.idle_read();
            // capacity растет - вырастает rss у процесса.
            self.client_last_messages_in_tx.shrink_if_needed();
            self.buffer.shrink_if_needed();
        }
    }

    pub(crate) async fn execute_server_roundtrip(
        &mut self,
        message: Option<&BytesMut>,
        server: &mut Server,
    ) -> Result<(), Error> {
        if !self.transaction_mode && self.session_xact_start.is_none() {
            self.session_xact_start = Some(crate::utils::clock::now());
        }
        let message = message.unwrap_or(&self.buffer);

        // Send message with timeout
        server
            .send_and_flush_timeout(message, Duration::from_secs(5))
            .await?;

        // Debug log: client -> server
        log_client_to_server(&self.addr_str, server.get_process_id(), message);

        // Relay the backend's response for the message we just sent.
        self.relay_response(server).await
    }

    async fn recv_server_response_or_client_disconnect(
        &mut self,
        server: &mut Server,
    ) -> Result<BytesMut, Error> {
        let mut watch_client = self.read.buffer().is_empty();
        loop {
            if !watch_client || server.is_data_available() {
                return server
                    .recv(&mut self.write, Some(&mut self.server_parameters))
                    .await;
            }

            tokio::select! {
                biased;
                // wait_server_data (NOT raw-socket server_readable): the
                // response may already sit inside the backend BufStream
                // buffer - e.g. the piggybacked SET + first client query
                // produce two pipelined replies that the kernel often
                // delivers in one read, so after the SET reply is swallowed
                // the client query's reply is buffered and the raw socket
                // never becomes readable again.
                _ = server.wait_server_data() => {
                    return server
                        .recv(&mut self.write, Some(&mut self.server_parameters))
                        .await;
                }
                client_read = self.read.fill_buf() => {
                    match client_read {
                        Ok([]) => {
                            server.mark_bad("client disconnected while waiting for server response");
                            return Err(Error::SocketError(
                                "client disconnected while waiting for server response".to_string(),
                            ));
                        }
                        Ok(_) => {
                            watch_client = false;
                        }
                        Err(err) => {
                            server.mark_bad("client read failed while waiting for server response");
                            return Err(Error::SocketError(format!(
                                "Error reading from client while waiting for server response: {err:?}"
                            )));
                        }
                    }
                }
            }
        }
    }

    /// Relay one backend response stream to the client, draining until the
    /// backend signals no more data is available (`!is_data_available()`).
    ///
    /// Factored out of [`Client::execute_server_roundtrip`] (anti-F6): the
    /// reorder / close-complete / fast-release bookkeeping is subtle enough
    /// that duplicating it in the piggyback path would be exactly the kind of
    /// drift that bred the F6 / batch-parse response-count regressions. The
    /// caller is responsible for having already flushed the request bytes to
    /// the backend; this method only reads + forwards the reply.
    ///
    /// MUST stay byte-for-byte behaviour-identical to the original inline
    /// loop: same recv, same ParseComplete reorder, same pending CloseComplete
    /// insertion, same fast-release condition, same error handling.
    pub(crate) async fn relay_response(&mut self, server: &mut Server) -> Result<(), Error> {
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();

        // Single initial state update
        self.stats.active_idle();

        // Read all data the server has to offer, which can be multiple messages
        // buffered in 8 KiB chunks.
        loop {
            let mut response = match self.recv_server_response_or_client_disconnect(server).await {
                Ok(msg) => msg,
                Err(err) => {
                    if !server.is_bad() {
                        server.wait_available().await;
                    }
                    let mut msg = String::with_capacity(64);
                    use std::fmt::Write;
                    let _ = write!(
                        msg,
                        "server recv failed during client {} roundtrip: {:?}",
                        self.addr, err
                    );
                    server.mark_bad(&msg);
                    return Err(err);
                }
            };
            let rejected_prepared_statement_names = server.take_rejected_prepared_statement_names();
            if !rejected_prepared_statement_names.is_empty() {
                self.drop_rejected_prepared_cache_entries(&rejected_prepared_statement_names);
            }

            // Insert pending ParseComplete messages based on batch_operations order
            // This ensures ParseComplete messages are inserted in the correct position
            // relative to other responses (ParameterDescription, BindComplete, etc.)
            if !self.prepared.batch_operations.is_empty()
                && !self.prepared.skipped_parses.is_empty()
            {
                let append_trailing_pending = server.is_async() && server.expected_responses() == 0;
                response = self.reorder_parse_complete_responses(response, append_trailing_pending);
            }

            // Insert pending CloseComplete messages after last CloseComplete from server
            if self.prepared.pending_close_complete > 0 {
                let (new_response, inserted) = insert_close_complete_after_last_close_complete(
                    response,
                    self.prepared.pending_close_complete,
                );
                response = new_response;
                self.prepared.pending_close_complete -= inserted;
            }

            // Debug log: server -> client (after all modifications to show what client actually receives)
            log_server_to_client(&self.addr_str, server.get_process_id(), &response);

            if self.transaction_mode
                && !self.sql_prepare_session_pinned
                && server.cleanup_state.needs_cleanup_prepare
                && response_contains_sql_prepare_command_complete(&response)
            {
                self.sql_prepare_session_pinned = true;
            }

            let can_fast_release = self.transaction_mode && !self.sql_prepare_session_pinned;

            // Fast path: early release check before expensive operations
            // This is the most common case in transaction mode
            // Don't use fast_release when there are pending prepared statement operations
            // to avoid protocol violations if client disconnects before receiving the response
            if can_fast_release
                && !server.is_data_available()
                && !server.in_transaction()
                && !server.in_copy_mode()
                && !server.is_async()
                && self.prepared.skipped_parses.is_empty()
                && self.prepared.pending_close_complete == 0
            {
                self.client_last_messages_in_tx.put(&response[..]);
                break;
            }

            // Write response to client
            self.stats.active_write();
            if let Err(err_write) =
                write_all_flush_timeout(&mut self.write, &response, write_timeout).await
            {
                warn!(
                    "[{}@{} #c{}] write to client failed pid={}: {err_write}",
                    self.username,
                    self.pool_name,
                    self.connection_id,
                    server.get_process_id()
                );
                if server.is_data_available() || server.is_async() || server.in_copy_mode() {
                    server.mark_bad(
                        format!(
                            "failed to flush response to client {}: {:?}",
                            self.addr, err_write
                        )
                        .as_str(),
                    );
                } else if !server.is_bad() {
                    server.wait_available().await;
                    // The backend produced a complete response and is parked
                    // at ReadyForQuery - only the client write failed. Run
                    // the checkin cleanup (including the release query) so
                    // the healthy backend can be reused instead of being
                    // closed by the recycle-safety check for an unconfirmed
                    // release round trip. A cleanup failure marks the
                    // backend bad; a cancellation mid-cleanup leaves the
                    // pending flag armed so Object::drop closes the backend.
                    if !server.is_bad() {
                        if let Err(cleanup_err) = server.finalize_checkin().await {
                            warn!(
                                "finalize_checkin after client write failure failed pid={}: {}",
                                server.get_process_id(),
                                cleanup_err
                            );
                        }
                    }
                }
                return Err(err_write);
            }

            self.stats.active_idle();

            // Early exit check
            if !server.is_data_available() {
                break;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[cfg(unix)]
mod migration_idle_read_tests {
    use super::*;
    use crate::app::server::publish_migration_in_progress;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    #[serial_test::serial(migration_globals)]
    async fn idle_read_wakes_on_migration_notice_without_client_bytes() {
        publish_migration_in_progress(true);

        let (client, _peer) = tokio::io::duplex(64);
        let mut read = BufReader::new(client);
        let mut read_buf = BytesMut::new();
        let mut task = tokio::spawn(async move {
            read_idle_message_or_migration_notice(&mut read, &mut read_buf, u64::MAX, false, true)
                .await
        });

        let mut result = None;
        for _ in 0..50 {
            MIGRATION_NOTIFY.notify_waiters();
            match tokio::time::timeout(Duration::from_millis(20), &mut task).await {
                Ok(joined) => {
                    result = Some(joined.expect("idle read task panicked"));
                    break;
                }
                Err(_) => continue,
            }
        }

        publish_migration_in_progress(false);

        match result
            .expect("idle read did not wake on migration notice")
            .unwrap()
        {
            IdleClientRead::MigrationRequested => {}
            IdleClientRead::Message(message) => {
                panic!("expected migration request, got client message {message:?}")
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(migration_globals)]
    async fn idle_read_observes_migration_already_in_progress_without_notify() {
        publish_migration_in_progress(true);

        let (client, _peer) = tokio::io::duplex(64);
        let mut read = BufReader::new(client);
        let mut read_buf = BytesMut::new();
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            read_idle_message_or_migration_notice(&mut read, &mut read_buf, u64::MAX, false, true),
        )
        .await;

        publish_migration_in_progress(false);

        match result
            .expect("migration already in progress must not wait for a fresh Notify")
            .unwrap()
        {
            IdleClientRead::MigrationRequested => {}
            IdleClientRead::Message(message) => {
                panic!("expected migration request, got client message {message:?}")
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(migration_globals)]
    async fn idle_read_consumes_client_message_when_migration_wake_disabled() {
        publish_migration_in_progress(true);

        let (client, mut peer) = tokio::io::duplex(128);
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            peer.write_all(&simple_query("SELECT 1")).await.unwrap();
        });

        let mut read = BufReader::new(client);
        let mut read_buf = BytesMut::new();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            read_idle_message_or_migration_notice(&mut read, &mut read_buf, u64::MAX, false, false),
        )
        .await;

        publish_migration_in_progress(false);
        writer.await.expect("writer task panicked");

        match result
            .expect("migration-disabled idle read must continue reading client bytes")
            .unwrap()
        {
            IdleClientRead::Message(message) => assert_eq!(message[0], b'Q'),
            IdleClientRead::MigrationRequested => {
                panic!("migration wake must be disabled while deferred client state is pending")
            }
        }
    }

    #[test]
    fn idle_migration_wake_call_site_excludes_deferred_begin() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };

        let wake_flag_pos = impl_src
            .find("let migration_wake_enabled =")
            .expect("idle loop must compute a migration wake guard before reading");
        let wake_flag_body = &impl_src[wake_flag_pos
            ..wake_flag_pos
                + impl_src[wake_flag_pos..]
                    .find(";")
                    .expect("migration wake guard should be a statement")];
        assert!(
            wake_flag_body.contains("self.client_pending_begin.is_none()"),
            "migration wake must be disabled while a deferred simple BEGIN is pending"
        );
        assert!(
            wake_flag_body.contains("deferred_extended_begin.is_none()"),
            "migration wake must be disabled while a deferred extended BEGIN is pending"
        );

        let call_pos = impl_src
            .find("read_idle_message_or_migration_notice(")
            .expect("idle read helper call should exist");
        let call_window = &impl_src[call_pos..call_pos + 260.min(impl_src.len() - call_pos)];
        assert!(
            call_window.contains("migration_wake_enabled"),
            "idle read helper must receive the migration wake guard"
        );
    }

    #[test]
    fn failed_idle_migration_attempt_disables_future_wakeups() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };

        assert!(
            impl_src.contains("let mut migration_wake_allowed = true;"),
            "idle loop must track clients that should stop receiving sticky migration wakes"
        );

        // The migration attempt awaits a channel slot (bounded by the absolute
        // migration deadline) instead of dropping the client on a full channel.
        let reserve_pos = impl_src
            .find("let drop_reason: Option<&'static str> = match tokio::time::timeout_at(")
            .expect("idle migration attempt must be bounded by timeout_at");
        let reserve_window =
            &impl_src[reserve_pos..reserve_pos + 400.min(impl_src.len() - reserve_pos)];
        assert!(
            reserve_window.contains("deadline") && reserve_window.contains("tx.reserve()"),
            "idle migration must await a channel slot bounded by the deadline, not try_reserve-and-drop"
        );
        assert!(
            impl_src.contains("crate::app::server::migration_deadline()"),
            "the reserve wait must be bounded by the published absolute migration deadline"
        );

        // Every non-success outcome funnels through one explicit drop path that
        // disables future migration wakes and records the loss for observability.
        let drop_pos = impl_src
            .find("if let Some(reason) = drop_reason {")
            .expect("failed migration outcomes must funnel through an explicit drop path");
        let drop_branch = &impl_src[drop_pos..drop_pos + 600.min(impl_src.len() - drop_pos)];
        assert!(
            drop_branch.contains("migration_wake_allowed = false"),
            "a client that fails to migrate must not rearm immediate migration wakes"
        );
        assert!(
            impl_src.contains("record_migration_client_dropped(reason)"),
            "a dropped (unmigrated) client must be counted for observability"
        );

        // The three bounded drop reasons must stay explicit.
        for reason in ["deadline", "channel_closed", "prepare_failed"] {
            assert!(
                impl_src.contains(&format!("Some(\"{reason}\")")),
                "migration drop reason '{reason}' must stay explicit"
            );
        }

        assert!(
            impl_src.contains("prepare_migration failed"),
            "prepare failure branch must stay explicit"
        );

        let wake_flag_pos = impl_src
            .find("let migration_wake_enabled =")
            .expect("idle loop must compute a migration wake guard before reading");
        let wake_flag_body = &impl_src[wake_flag_pos
            ..wake_flag_pos
                + impl_src[wake_flag_pos..]
                    .find(";")
                    .expect("migration wake guard should be a statement")];
        assert!(
            wake_flag_body.contains("migration_wake_allowed"),
            "idle read helper must receive the per-client failed-migration guard"
        );
    }
}

#[cfg(test)]
mod checkout_error_tests {
    #[test]
    fn resource_exhausted_message_suppresses_backend_detail() {
        let raw = "connect 10.0.12.34:5432 failed: Too many open files";
        let msg = super::checkout_resource_exhausted_client_message(raw);

        assert!(msg.contains("local resource exhausted"));
        assert!(!msg.contains("10.0.12.34"));
        assert!(!msg.contains("Too many open files"));
    }

    #[test]
    fn checkout_failure_errors_are_deadline_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let branch_start = impl_src
            .find(
                "Err(err) => {\n                            // Client is attempting to get results",
            )
            .expect("checkout failure branch not found");
        let branch = &impl_src[branch_start..];
        let branch_end = branch
            .find("let checkout_us = connecting_at.elapsed")
            .expect("checkout success accounting should follow failure branch");
        let branch = &branch[..branch_end];

        let helper_start = impl_src
            .find("async fn write_checkout_error(")
            .expect("checkout error helper must exist");
        let helper_body = &impl_src[helper_start..];
        let helper_end = helper_body
            .find("\n    /// Handle a connected and authenticated client")
            .expect("client handler should follow checkout helper");
        let helper_body = &helper_body[..helper_end];

        assert!(
            helper_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "checkout failure helper must use proxy_copy_data_timeout"
        );
        assert!(
            helper_body.contains("error_response_timeout("),
            "checkout failure client errors must be deadline-bound"
        );
        assert!(
            branch.contains("self.write_checkout_error("),
            "checkout failure branches must route client errors through the bounded helper"
        );
        assert!(
            !branch.contains("error_response("),
            "checkout failure branches must not use unbounded client error writes"
        );
    }
}

#[cfg(test)]
mod extended_batch_metadata_cap_tests {
    use super::{
        enforce_extended_batch_buffer_cap, enforce_extended_batch_metadata_cap,
        extended_batch_metadata_bytes, EXTENDED_BATCH_BUFFER_CAP, EXTENDED_BATCH_METADATA_CAP,
    };

    #[test]
    fn buffer_cap_admits_large_single_bind_but_rejects_unbounded_accumulation() {
        // A file upload arrives as ONE large Bind (npgsql sends the bytea
        // parameter inline; an attachment over ~10MB produced an 18_632_275-byte
        // Bind in production) followed immediately by Sync. That legitimate
        // message must fit under the pending-buffer cap.
        enforce_extended_batch_buffer_cap(1108, 18_632_275, "Bind")
            .expect("a single large Bind (file upload) must fit under the cap");

        // A single wire message is itself bounded by MAX_MESSAGE_SIZE (256MB)
        // at read time, so the cap equals that ceiling: only pipelining
        // without Sync/Flush can exceed it.
        assert_eq!(
            EXTENDED_BATCH_BUFFER_CAP,
            crate::messages::MAX_MESSAGE_SIZE as usize
        );
        let err = enforce_extended_batch_buffer_cap(EXTENDED_BATCH_BUFFER_CAP, 1, "Bind")
            .expect_err("accumulation past the cap must still be rejected");
        assert!(err.to_string().contains("pending buffer"));
    }

    #[test]
    fn metadata_cap_rejects_skipped_parse_growth_before_synthetic_response_allocation() {
        let per_skip = extended_batch_metadata_bytes(1, 1);
        assert!(per_skip > 0);
        let max_skips = EXTENDED_BATCH_METADATA_CAP / per_skip;

        assert!(
            enforce_extended_batch_metadata_cap(max_skips - 1, max_skips - 1, 1, 1, "cached Parse")
                .is_ok(),
            "the last entry inside the metadata budget should be accepted"
        );

        let err = enforce_extended_batch_metadata_cap(max_skips, max_skips, 1, 1, "cached Parse")
            .expect_err("one more cached Parse must exceed the metadata budget");
        let msg = err.to_string();
        assert!(msg.contains("pending metadata"));
        assert!(msg.contains("cached Parse"));
    }

    #[test]
    fn metadata_cap_saturates_counting_overflows() {
        let err = enforce_extended_batch_metadata_cap(usize::MAX, usize::MAX, 1, 1, "cached Parse")
            .expect_err("saturating arithmetic must still reject absurd metadata counts");
        assert!(err.to_string().contains("pending metadata"));
    }
}

#[cfg(test)]
mod internal_round_trip_timeout_tests {
    use super::*;

    #[test]
    fn pooler_check_query_backend_probe_uses_single_deadline() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_pooler_check_query")
            .expect("pooler_check_query handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle simple query")
            .expect("simple-query handler should follow pooler_check_query");
        let body = &body[..end];

        assert!(
            body.contains("let deadline = tokio::time::Instant::now() + HOUSEKEEPING_TIMEOUT"),
            "pooler_check_query backend probe must use the shared housekeeping deadline"
        );
        assert!(
            body.contains("timeout_at(deadline, conn.send_and_flush(message))"),
            "pooler_check_query send must be covered by the deadline"
        );
        assert!(
            body.contains("timeout_at(deadline, conn.recv(writer, None))"),
            "pooler_check_query recv drain must be covered by the deadline"
        );
        assert!(
            !body.contains("conn.send_and_flush(message).await"),
            "pooler_check_query must not use a bare send await"
        );
        assert!(
            !body.contains("conn.recv(writer, None).await"),
            "pooler_check_query must not use a bare recv await"
        );
    }

    #[test]
    fn pooler_check_query_response_cap_rejects_oversize_before_append() {
        let mut response =
            BytesMut::from(&vec![b'a'; POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES - 1][..]);
        let err = append_pooler_check_query_response(&mut response, b"bc").unwrap_err();

        assert!(
            err.contains("exceeded"),
            "oversize response must produce a clear error, got {err}"
        );
        assert_eq!(
            response.len(),
            POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES - 1,
            "rejected bytes must not be appended to the retained response buffer"
        );
    }

    #[test]
    fn pooler_check_query_handler_bounds_response_before_cache_set() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_pooler_check_query")
            .expect("pooler_check_query handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle simple query")
            .expect("simple-query handler should follow pooler_check_query");
        let body = &body[..end];

        let append_idx = body
            .find("append_pooler_check_query_response")
            .expect("pooler_check_query response must use bounded append");
        let cache_idx = body
            .find("pool.check_query_cache\n                .set(")
            .expect("pooler_check_query response cache set not found");

        assert!(
            append_idx < cache_idx,
            "pooler_check_query must bound accumulated response bytes before caching"
        );
    }

    #[test]
    fn pooler_check_query_recv_streaming_path_is_capped_before_materialization() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_pooler_check_query")
            .expect("pooler_check_query handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle simple query")
            .expect("simple-query handler should follow pooler_check_query");
        let body = &body[..end];

        let override_idx = body
            .find("conn.max_message_size = POOLER_CHECK_QUERY_MAX_RESPONSE_BYTES.saturating_sub(1) as i32")
            .expect("pooler_check_query must force large DataRow streaming before its total response cap");
        let capped_writer_idx = body
            .find("BufferingWriter::capped")
            .expect("pooler_check_query recv overflow writer must be capped");
        let recv_idx = body
            .find("conn.recv(writer, None)")
            .expect("pooler_check_query recv not found");
        let restore_idx = body
            .rfind("conn.max_message_size = original_max_message_size")
            .expect("pooler_check_query must restore the backend stream threshold");

        assert!(
            override_idx < capped_writer_idx && capped_writer_idx < recv_idx && recv_idx < restore_idx,
            "pooler_check_query must force streaming at the response cap, use a capped writer, then restore max_message_size"
        );
    }

    #[test]
    fn pooler_check_query_cache_miss_evicts_canceled_backend() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_pooler_check_query")
            .expect("pooler_check_query handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle simple query")
            .expect("simple-query handler should follow pooler_check_query");
        let body = &body[..end];

        let consume_idx = body
            .find("canceled_pids_consume(conn.get_process_id())")
            .expect("pooler_check_query cache miss must consume cancel quarantine marker");
        let cleanup_idx = body
            .find("conn.checkin_cleanup().await")
            .expect("pooler_check_query must clean backend before probe");
        let window = &body[consume_idx..cleanup_idx];

        assert!(
            window.contains("CancelMarker::Fresh"),
            "pooler_check_query cache miss must branch on a FRESH cancel marker"
        );
        assert!(
            window.contains("mark_bad"),
            "pooler_check_query cache miss must evict a FRESH-quarantined backend"
        );
        assert!(
            window.contains("continue"),
            "pooler_check_query cache miss must retry after evicting a FRESH-quarantined backend"
        );
    }

    #[test]
    fn pooler_check_query_releases_backend_before_client_write() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_pooler_check_query")
            .expect("pooler_check_query handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle simple query")
            .expect("simple-query handler should follow pooler_check_query");
        let body = &body[..end];

        let finish_idx = body
            .find("conn.finish_internal_round_trip();")
            .expect("pooler_check_query must finish the internal backend roundtrip");
        let drop_idx = body
            .find("drop(conn);")
            .expect("pooler_check_query must release the backend before writing to the client");
        let write_idx = body
            .find("write_all_flush_timeout(&mut self.write, &response")
            .expect("pooler_check_query client response write must be deadline-bound");
        let cache_idx = body
            .find("pool.check_query_cache\n                .set(")
            .expect("pooler_check_query response cache set not found");

        assert!(
            finish_idx < drop_idx && drop_idx < write_idx && write_idx < cache_idx,
            "pooler_check_query cache miss must finish and release the backend before bounded client response write and cache insertion"
        );
        assert!(
            body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "pooler_check_query client response writes must use proxy_copy_data_timeout"
        );
        assert!(
            !body.contains("write_all_flush(&mut self.write, &response"),
            "pooler_check_query must not use an unbounded client write while a backend is checked out"
        );
    }

    #[test]
    fn pooler_check_query_cache_hit_write_is_deadline_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_pooler_check_query")
            .expect("pooler_check_query handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle simple query")
            .expect("simple-query handler should follow pooler_check_query");
        let body = &body[..end];

        let hit_start = body
            .find("if let Some(cached) = pool.check_query_cache.get")
            .expect("pooler_check_query cache-hit branch not found");
        let hit_body = &body[hit_start..];
        let hit_end = hit_body
            .find("return Ok(());")
            .expect("cache-hit branch should return after writing cached response");
        let hit_body = &hit_body[..hit_end];

        assert!(
            hit_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "pooler_check_query cache-hit writes must use proxy_copy_data_timeout"
        );
        assert!(
            hit_body.contains(
                "write_all_flush_timeout(&mut self.write, cached.as_ref(), write_timeout)"
            ),
            "pooler_check_query cache-hit writes must not wait forever on slow clients"
        );
        assert!(
            !hit_body.contains("write_all_flush(&mut self.write, cached.as_ref())"),
            "pooler_check_query cache-hit writes must not use an unbounded client write"
        );
    }

    #[test]
    fn no_server_synthetic_replies_are_deadline_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let extended_start = impl_src
            .find("async fn try_handle_deferred_extended_begin")
            .expect("deferred extended BEGIN handler not found");
        let extended_body = &impl_src[extended_start..];
        let extended_end = extended_body
            .find("\n    async fn try_handle_without_server(")
            .expect("without-server handler should follow deferred extended BEGIN");
        let extended_body = &extended_body[..extended_end];
        let extended_write_idx = extended_body
            .find("write_all_flush_timeout(")
            .expect("deferred extended BEGIN synthetic response must be deadline-bound");
        let extended_write_call = &extended_body[extended_write_idx..];
        let extended_write_call = &extended_write_call[..extended_write_call
            .find(".await?")
            .expect("deferred extended BEGIN bounded write should be awaited")];
        assert!(
            extended_write_call.contains("SYNTHETIC_EXTENDED_BEGIN_RESPONSE")
                && extended_write_call.contains("write_timeout"),
            "deferred extended BEGIN bounded write must send the synthetic response with the configured timeout"
        );
        let extended_pending_idx = extended_body
            .find("self.client_pending_begin = Some(simple_begin_message())")
            .expect("deferred extended BEGIN must still set client_pending_begin");
        assert!(
            extended_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "deferred extended BEGIN writes must use proxy_copy_data_timeout"
        );
        assert!(
            extended_write_idx < extended_pending_idx,
            "deferred extended BEGIN must set client_pending_begin only after the bounded client write succeeds"
        );
        assert!(
            !extended_body
                .contains("write_all_flush(&mut self.write, SYNTHETIC_EXTENDED_BEGIN_RESPONSE)"),
            "deferred extended BEGIN must not use an unbounded client write"
        );

        let fast_path_start = impl_src
            .find("async fn try_handle_without_server")
            .expect("without-server handler not found");
        let fast_path_body = &impl_src[fast_path_start..];
        let fast_path_end = fast_path_body
            .find("\n    /// Serve a `general.pooler_check_query`")
            .expect("pooler check handler should follow fast path");
        let fast_path_body = &fast_path_body[..fast_path_end];
        assert!(
            fast_path_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "synthetic DEALLOCATE writes must use proxy_copy_data_timeout"
        );
        let deallocate_write_idx = fast_path_body
            .find("write_all_flush_timeout(")
            .expect("synthetic DEALLOCATE ack must be deadline-bound");
        let deallocate_write_call = &fast_path_body[deallocate_write_idx..];
        let deallocate_write_call = &deallocate_write_call[..deallocate_write_call
            .find(".await?")
            .expect("synthetic DEALLOCATE bounded write should be awaited")];
        assert!(
            deallocate_write_call.contains("&SIMPLE_DEALLOCATE_NAMED_ACK")
                && deallocate_write_call.contains("write_timeout"),
            "synthetic DEALLOCATE ack must not wait forever on a slow client"
        );
        assert!(
            !fast_path_body
                .contains("write_all_flush(&mut self.write, &SIMPLE_DEALLOCATE_NAMED_ACK)"),
            "synthetic DEALLOCATE ack must not use an unbounded client write"
        );

        let simple_begin_start = impl_src
            .find("if is_standalone_begin(&message) && self.client_pending_begin.is_none()")
            .expect("standalone simple BEGIN fast path not found");
        let simple_begin_body = &impl_src[simple_begin_start..];
        let simple_begin_end = simple_begin_body
            .find("// Check if we have a pending BEGIN to send with this query")
            .expect("pending-BEGIN send path should follow simple BEGIN fast path");
        let simple_begin_body = &simple_begin_body[..simple_begin_end];
        let simple_write_idx = simple_begin_body
            .find("write_all_flush_timeout(")
            .expect("simple deferred BEGIN synthetic response must be deadline-bound");
        let simple_write_call = &simple_begin_body[simple_write_idx..];
        let simple_write_call = &simple_write_call[..simple_write_call
            .find(".await?")
            .expect("simple deferred BEGIN bounded write should be awaited")];
        assert!(
            simple_write_call.contains("SYNTHETIC_BEGIN_RESPONSE")
                && simple_write_call.contains("write_timeout"),
            "simple deferred BEGIN bounded write must send the synthetic response with the configured timeout"
        );
        let simple_pending_idx = simple_begin_body
            .find("self.client_pending_begin = Some(message)")
            .expect("simple deferred BEGIN must still store the pending BEGIN");
        assert!(
            simple_begin_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "simple deferred BEGIN writes must use proxy_copy_data_timeout"
        );
        assert!(
            simple_write_idx < simple_pending_idx,
            "simple deferred BEGIN must set client_pending_begin only after the bounded client write succeeds"
        );
        assert!(
            !simple_begin_body
                .contains("write_all_flush(&mut self.write, SYNTHETIC_BEGIN_RESPONSE)"),
            "simple deferred BEGIN must not use an unbounded client write"
        );
    }

    #[test]
    fn deferred_begin_recv_drain_uses_deadline() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let lines: Vec<&str> = impl_src.lines().collect();
        let begin_idx = lines
            .iter()
            .position(|l| l.contains("if let Some(begin_msg) = pending_begin {"))
            .expect("deferred-BEGIN block not found");
        let block_end = lines[begin_idx..]
            .iter()
            .position(|l| l.contains("query_start_at = now();"))
            .map(|i| begin_idx + i)
            .expect("deferred-BEGIN block end not found");
        let body = lines[begin_idx..=block_end].join("\n");

        assert!(
            body.contains("let deadline = tokio::time::Instant::now() + HOUSEKEEPING_TIMEOUT"),
            "deferred BEGIN must establish a shared housekeeping deadline"
        );
        assert!(
            body.contains("timeout_at(") && body.contains("server.recv("),
            "deferred BEGIN recv drain must be covered by the deadline"
        );
        assert!(
            !body.contains(".recv(&mut tokio::io::sink(), Some(&mut self.server_parameters))\n                            .await"),
            "deferred BEGIN must not use a bare recv await"
        );
    }

    #[test]
    fn server_dead_idle_in_transaction_releases_before_bounded_client_error() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("Ok(NextClientMessage::ServerDead) =>")
            .expect("ServerDead idle-in-transaction branch not found");
        let body = &impl_src[start..];
        let end = body
            .find("Err(err) =>")
            .expect("ServerDead branch should be followed by client-error branch");
        let body = &body[..end];

        let mark_bad_idx = body
            .find("mark_bad(\"server closed while client idle in transaction\")")
            .expect("ServerDead branch must mark the backend bad");
        let connected_false_idx = body
            .find("self.connected_to_server = false")
            .expect("ServerDead branch must detach client state before writing");
        let release_idx = body
            .find("self.release();")
            .expect("ServerDead branch must release cancel routing before writing");
        let drop_idx = body
            .find("drop(conn);")
            .expect("ServerDead branch must drop the checked-out backend before writing");
        let write_idx = body
            .find("error_response_timeout(")
            .expect("ServerDead client error write must be timeout-bound");

        assert!(
            mark_bad_idx < connected_false_idx
                && connected_false_idx < release_idx
                && release_idx < drop_idx
                && drop_idx < write_idx,
            "ServerDead must evict/release the bad backend before writing the client error"
        );
        assert!(
            body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "ServerDead client error write must use proxy_copy_data_timeout"
        );
        assert!(
            !body.contains("error_response(\n"),
            "ServerDead must not use an unbounded client error write"
        );
    }

    #[test]
    fn backend_timeout_errors_release_and_drop_before_client_error_write() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let action_match_start = impl_src
            .find("let action = match action_result {")
            .expect("action_result match not found");
        let action_match = &impl_src[action_match_start..];
        let error_branch_start = action_match
            .find("Err(err) => {")
            .expect("inner handler error branch not found");
        let error_branch = &action_match[error_branch_start..];
        let error_branch_end = error_branch
            .find("return Err(err);")
            .expect("inner handler error branch must return err");
        let error_branch = &error_branch[..error_branch_end];

        let classify_idx = error_branch
            .find("backend_timeout_client_error(&err)")
            .expect("inner errors must classify backend timeout client error");
        let release_idx = error_branch
            .find("self.release_after_inner_handler_error();")
            .expect("inner errors must release cancel routing");
        let drop_idx = error_branch
            .find("drop(conn);")
            .expect("timeout client error path must drop the checked-out backend");
        let write_idx = error_branch
            .find("error_response_timeout(")
            .expect("timeout client error must use a bounded write");

        assert!(
            classify_idx < release_idx && release_idx < drop_idx && drop_idx < write_idx,
            "backend timeout client error must be classified, release cancel routing, drop backend, then bounded-write"
        );
        assert!(
            !error_branch.contains("error_response_terminal("),
            "inner error branch must not use unbounded terminal writes"
        );
    }

    #[test]
    fn backend_timeout_inner_helpers_do_not_write_client_errors_while_checked_out() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        for (start_marker, end_marker) in [
            (
                "async fn handle_simple_query(",
                "\n    async fn write_synthetic_parse_completes",
            ),
            (
                "async fn flush_copy_buffer_with_timeout(",
                "\n    async fn recv_copy_completion_with_timeout",
            ),
            (
                "async fn recv_copy_completion_with_timeout(",
                "\n    async fn handle_copy_data",
            ),
            (
                "pub(crate) async fn execute_server_roundtrip(",
                "\n    async fn recv_server_response_or_client_disconnect",
            ),
        ] {
            let start = impl_src.find(start_marker).expect(start_marker);
            let body = &impl_src[start..];
            let end = body.find(end_marker).expect(end_marker);
            let body = &body[..end];
            assert!(
                !body.contains("error_response_terminal("),
                "{start_marker} must not write unbounded client errors while the backend is checked out"
            );
        }
    }

    #[test]
    fn idle_shutdown_errors_are_deadline_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let helper_start = impl_src
            .find("async fn write_shutdown_error_and_disconnect(")
            .expect("shutdown helper must exist");
        let helper_body = &impl_src[helper_start..];
        let helper_end = helper_body
            .find("\n    /// Handle a connected and authenticated client")
            .expect("client handler should follow shutdown helper");
        let helper_body = &helper_body[..helper_end];
        assert!(
            helper_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "shutdown client error writes must use proxy_copy_data_timeout"
        );
        assert!(
            helper_body.contains("error_response_timeout("),
            "shutdown client errors must be deadline-bound"
        );
        assert!(
            helper_body.contains("self.stats.disconnect();"),
            "shutdown helper must count the client disconnect even when the error write times out"
        );
        assert!(
            !helper_body.contains("error_response_terminal("),
            "shutdown helper must not use unbounded terminal writes"
        );

        let client_loop_start = impl_src
            .find("pub async fn handle(")
            .expect("client handle loop not found");
        let client_loop = &impl_src[client_loop_start..];
        let client_loop_end = client_loop
            .find("\n    pub(crate) async fn execute_server_roundtrip")
            .expect("post-handle marker not found");
        let client_loop = &client_loop[..client_loop_end];
        assert!(
            client_loop.contains("self.write_shutdown_error_and_disconnect().await?;"),
            "idle shutdown paths must use the bounded shutdown helper"
        );
        assert!(
            !client_loop.contains("error_response_terminal("),
            "idle shutdown paths must not use unbounded terminal writes"
        );
    }

    #[test]
    fn copy_done_fail_recv_disables_async_short_circuit() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_copy_done_fail(")
            .expect("COPY completion handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle a connected and authenticated client")
            .expect("client handler should follow COPY completion handler");
        let body = &body[..end];
        let recv_idx = body
            .find("recv_copy_completion_with_timeout(server).await?")
            .expect("COPY completion handler must drain backend response");
        let before_recv = &body[..recv_idx];

        assert!(
            before_recv.contains("server.set_async_mode(false);"),
            "CopyDone/CopyFail completion must disable async recv short-circuit before draining"
        );
        assert!(
            before_recv.contains("server.set_expected_responses(0);"),
            "CopyDone/CopyFail completion must reset expected responses before draining"
        );
    }

    #[test]
    fn copy_done_fail_recv_uses_housekeeping_deadline() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_copy_done_fail(")
            .expect("COPY completion handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle a connected and authenticated client")
            .expect("client handler should follow COPY completion handler");
        let body = &body[..end];

        assert!(
            body.contains("recv_copy_completion_with_timeout(server).await?"),
            "COPY FROM completion must drain the backend response through a bounded helper"
        );
        assert!(
            !body.contains(
                ".recv(&mut self.write, Some(&mut self.server_parameters))\n            .await?"
            ),
            "COPY FROM completion must not wait on a bare backend recv while holding the checkout"
        );
    }

    #[test]
    fn copy_done_fail_client_write_is_deadline_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_copy_done_fail(")
            .expect("COPY completion handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle a connected and authenticated client")
            .expect("client handler should follow COPY completion handler");
        let body = &body[..end];

        assert!(
            body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "COPY completion client write must use proxy_copy_data_timeout"
        );
        assert!(
            body.contains("write_all_flush_timeout(&mut self.write, &response"),
            "COPY completion response must be deadline-bound while a backend is checked out"
        );
        assert!(
            !body.contains("write_all_flush(&mut self.write, &response"),
            "COPY completion response must not use an unbounded client write"
        );

        let recv_idx = body
            .find("recv_copy_completion_with_timeout(server).await?")
            .expect("COPY completion handler must drain backend response first");
        let write_idx = body
            .find("write_all_flush_timeout(&mut self.write, &response")
            .expect("bounded COPY completion client write not found");
        let eviction_idx = body
            .find("server.send_deferred_eviction_closes().await?")
            .expect("COPY completion handler must continue backend cleanup after write");
        assert!(
            recv_idx < write_idx && write_idx < eviction_idx,
            "COPY completion response write must remain between backend drain and deferred eviction cleanup"
        );
    }

    #[test]
    fn copy_from_client_writes_use_send_deadline() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_copy_data(")
            .expect("COPY data handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle a connected and authenticated client")
            .expect("client handler should follow COPY completion handler");
        let body = &body[..end];

        assert!(
            body.contains("flush_copy_buffer_with_timeout(server).await?"),
            "COPY FROM client writes must use the deadline-bound flush helper"
        );
        assert!(
            !body.contains("server.send_and_flush(&self.buffer).await?"),
            "COPY FROM client writes must not use a bare server send await"
        );
    }

    #[test]
    fn fresh_zero_response_flush_finishes_without_backend_roundtrip() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_sync_flush(")
            .expect("Sync/Flush handler not found");
        let end = impl_src[start..]
            .find("\n    /// Handle CopyData")
            .map(|offset| start + offset)
            .expect("CopyData handler should follow Sync/Flush handler");
        let body = &impl_src[start..end];
        let async_snapshot = body
            .find("let was_async = server.is_async();")
            .expect("Flush must snapshot pre-existing async state");
        let enter_async = body
            .find("server.set_async_mode(true);")
            .expect("Flush must enter async mode");
        assert!(
            async_snapshot < enter_async,
            "pre-existing async state must be captured before Flush mutates it"
        );

        let branch_start = body
            .find("if expected == 0 && !was_async")
            .expect("fresh zero-response Flush must have an early no-backend-response branch");
        let branch_end = body[branch_start..]
            .find("self.execute_server_roundtrip(None, server).await?;")
            .map(|offset| branch_start + offset)
            .expect("roundtrip call should remain after zero-response branch");
        let zero_branch = &body[branch_start..branch_end];

        assert!(
            zero_branch.contains("self.write_synthetic_parse_completes().await?;"),
            "fresh zero-response Flush must still emit queued synthetic ParseComplete replies"
        );
        assert!(
            zero_branch.contains("server.set_async_mode(false);")
                && zero_branch.contains("server.set_expected_responses(0);"),
            "fresh zero-response Flush must clear async state before release"
        );
        assert!(
            zero_branch.contains("self.buffer.clear();")
                && zero_branch.contains("self.prepared.reset_batch();"),
            "fresh zero-response Flush must discard the frontend Flush frame and reset batch state"
        );
        assert!(
            !zero_branch.contains("execute_server_roundtrip"),
            "fresh zero-response Flush must not forward bare Flush to the backend"
        );
    }

    #[test]
    fn synthetic_parse_complete_write_is_deadline_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn write_synthetic_parse_completes")
            .expect("synthetic ParseComplete helper not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    async fn flush_copy_buffer_with_timeout")
            .expect("COPY flush helper should follow synthetic ParseComplete helper");
        let body = &body[..end];

        assert!(
            body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "synthetic ParseComplete writes must use proxy_copy_data_timeout"
        );
        assert!(
            body.contains("write_all_flush_timeout(&mut self.write, &synthetic_response"),
            "synthetic ParseComplete writes must be deadline-bound while a backend is checked out"
        );
        assert!(
            !body.contains("write_all_flush(&mut self.write, &synthetic_response"),
            "synthetic ParseComplete writes must not use an unbounded client write"
        );

        let write_idx = body
            .find("write_all_flush_timeout(&mut self.write, &synthetic_response")
            .expect("bounded synthetic ParseComplete write not found");
        let clear_idx = body
            .find("self.prepared.skipped_parses.clear();")
            .expect("synthetic ParseComplete helper must clear skipped parses after success");
        assert!(
            write_idx < clear_idx,
            "skipped Parse state must be retained if the bounded client write times out"
        );
    }

    #[test]
    fn idle_sync_clears_unexecuted_portal_cleanup_attribution() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_sync_flush(")
            .expect("Sync/Flush handler not found");
        let end = impl_src[start..]
            .find("\n    async fn write_synthetic_parse_completes")
            .map(|offset| start + offset)
            .expect("write_synthetic_parse_completes should follow Sync/Flush handler");
        let body = &impl_src[start..end];

        let roundtrip_idx = body
            .find("self.execute_server_roundtrip(None, server).await?;")
            .expect("Sync path must execute a backend roundtrip");
        let after_roundtrip = &body[roundtrip_idx..];
        let clear_idx = roundtrip_idx
            + after_roundtrip
                .find("self.prepared.clear_portal_cleanup_commands();")
                .expect("idle ReadyForQuery must clear unexecuted portal cleanup attribution");
        let reset_idx = roundtrip_idx
            + after_roundtrip
                .find("self.prepared.reset_batch();")
                .expect("Sync path must reset batch state");

        assert!(
            roundtrip_idx < clear_idx && clear_idx < reset_idx,
            "portal cleanup attribution must be cleared after backend transaction state is known \
             and before the batch is released"
        );
        assert!(
            body[roundtrip_idx..clear_idx].contains("!server.in_transaction()"),
            "portal cleanup attribution must only be cleared when ReadyForQuery leaves the backend idle"
        );
        assert!(
            body[roundtrip_idx..clear_idx].contains("code != 'H'"),
            "Flush-only async batches must not clear portal cleanup attribution before a later Execute"
        );
    }

    #[test]
    fn transaction_completion_clears_portal_cleanup_attribution_before_release() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("fn complete_transaction_if_needed(")
            .expect("transaction completion helper not found");
        let end = impl_src[start..]
            .find("\n    /// Ensure server is in copy mode")
            .map(|offset| start + offset)
            .expect("copy-mode helper should follow transaction completion helper");
        let body = &impl_src[start..end];

        let stats_idx = body
            .find("self.stats.transaction();")
            .expect("transaction completion stats marker not found");
        let clear_idx = body
            .find("self.prepared.clear_portal_cleanup_commands();")
            .expect("transaction completion must clear portal cleanup attribution");
        let break_idx = body
            .find("return true;")
            .expect("transaction-mode release marker not found");

        assert!(
            stats_idx < clear_idx && clear_idx < break_idx,
            "portal cleanup attribution must be cleared after ReadyForQuery leaves transaction \
             and before transaction-pool release"
        );
    }
}

#[cfg(test)]
mod pre_server_replay_tests {
    use super::*;

    #[test]
    fn transaction_loop_drains_replay_before_reading_socket() {
        let mut initial = Some(BytesMut::from(&b"P"[..]));
        let mut replay = VecDeque::from([BytesMut::from(&b"B"[..]), BytesMut::from(&b"D"[..])]);

        assert_eq!(
            take_queued_pre_server_message(&mut initial, &mut replay).unwrap(),
            BytesMut::from(&b"P"[..])
        );
        assert_eq!(
            take_queued_pre_server_message(&mut initial, &mut replay).unwrap(),
            BytesMut::from(&b"B"[..])
        );
        assert_eq!(
            take_queued_pre_server_message(&mut initial, &mut replay).unwrap(),
            BytesMut::from(&b"D"[..])
        );
        assert!(take_queued_pre_server_message(&mut initial, &mut replay).is_none());
    }
}

#[cfg(test)]
mod rejected_parse_rollback_tests {
    #[test]
    fn relay_response_rolls_back_rejected_parse_cache_before_reordering() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let relay_start = impl_src
            .find("pub(crate) async fn relay_response(")
            .expect("relay_response should exist");
        let relay_body = &impl_src[relay_start..];

        let take_idx = relay_body
            .find("server.take_rejected_prepared_statement_names()")
            .expect("relay_response must take rejected Parse names from Server");
        let drop_idx = relay_body
            .find("self.drop_rejected_prepared_cache_entries(&rejected_prepared_statement_names)")
            .expect("relay_response must roll back rejected Parse names from client cache");
        let reorder_idx = relay_body
            .find("self.reorder_parse_complete_responses")
            .expect("relay_response should still perform ParseComplete reordering");

        assert!(
            take_idx < drop_idx && drop_idx < reorder_idx,
            "rejected Parse cache rollback must run before synthetic ParseComplete reordering"
        );
    }
}

#[cfg(test)]
mod client_response_write_timeout_tests {
    #[test]
    fn relay_response_bounds_client_proxy_writes() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let relay_start = impl_src
            .find("pub(crate) async fn relay_response(")
            .expect("relay_response should exist");
        let relay_body = &impl_src[relay_start..];

        assert!(
            relay_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "client response relay writes must use the configured proxy copy timeout"
        );
        assert!(
            relay_body.contains("write_all_flush_timeout(&mut self.write, &response"),
            "client response relay must not wait forever on slow client TCP writes"
        );
        assert!(
            !relay_body.contains("write_all_flush(&mut self.write, &response"),
            "client response relay must not use an unbounded client write while holding a backend"
        );
    }

    #[test]
    fn fast_release_post_release_flush_is_timeout_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let flush_start = impl_src
            .find("let has_buffered_response = !self.client_last_messages_in_tx.is_empty()")
            .expect("post-release fast-response flush should exist");
        let flush_body = &impl_src[flush_start..];
        let flush_end = flush_body
            .find("// TransactionGuard dropped at end of block above")
            .expect("transaction guard comment should follow fast-response flush");
        let flush_body = &flush_body[..flush_end];

        assert!(
            flush_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "post-release fast-response flush must use proxy_copy_data_timeout"
        );
        assert!(
            flush_body.contains("write_all_flush_timeout(")
                && flush_body.contains("buffered_response")
                && flush_body.contains("write_timeout"),
            "post-release fast-response flush must not wait forever on a slow client"
        );
        assert!(
            !flush_body
                .contains("write_all_flush(&mut self.write, &self.client_last_messages_in_tx"),
            "post-release fast-response flush must not use an unbounded client write"
        );
    }

    #[test]
    fn relay_response_races_backend_recv_with_client_disconnect() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let helper_start = impl_src
            .find("async fn recv_server_response_or_client_disconnect(")
            .expect("client-disconnect receive helper should exist");
        let helper_body = &impl_src[helper_start..];
        let helper_end = helper_body
            .find("\n    /// Relay one backend response stream")
            .expect("relay_response should follow the receive helper");
        let helper_body = &helper_body[..helper_end];
        let relay_start = impl_src
            .find("pub(crate) async fn relay_response(")
            .expect("relay_response should exist");
        let relay_body = &impl_src[relay_start..];

        assert!(
            helper_body.contains("self.read.fill_buf()"),
            "backend response wait must peek the client read half without consuming pipelined bytes"
        );
        assert!(
            helper_body.contains("server.wait_server_data()"),
            "backend wait must see bytes already buffered in the BufStream, not just raw-socket \
             readiness - pipelined piggyback replies deadlock a readable()-based wait"
        );
        assert!(
            !helper_body.contains("server.server_readable()"),
            "raw-socket readable() is blind to BufStream-buffered response bytes"
        );
        assert!(
            helper_body.contains(
                "server.mark_bad(\"client disconnected while waiting for server response\")"
            ),
            "client EOF/RST while waiting for backend bytes must evict the checked-out backend"
        );
        assert!(
            helper_body.contains("watch_client = false"),
            "pipelined client bytes must remain buffered and disable the EOF watcher for this response"
        );
        assert!(
            relay_body.contains("self.recv_server_response_or_client_disconnect(server).await"),
            "relay_response must use the client-disconnect-aware backend recv helper"
        );
    }

    #[test]
    fn client_disconnect_watch_does_not_cancel_backend_frame_recv() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let helper_start = impl_src
            .find("async fn recv_server_response_or_client_disconnect(")
            .expect("client-disconnect receive helper should exist");
        let helper_body = &impl_src[helper_start..];
        let helper_end = helper_body
            .find("\n    /// Relay one backend response stream")
            .expect("relay_response should follow the receive helper");
        let helper_body = &helper_body[..helper_end];

        assert!(
            helper_body.contains("server.wait_server_data()"),
            "client-disconnect watcher must race only a cancel-safe backend data wait \
             (fill_buf peeks without consuming - safe to drop mid-poll)"
        );
        assert!(
            !helper_body.contains("result = server.recv("),
            "client-disconnect watcher must not poll server.recv inside select; \
             dropping that future can lose a partially-read backend frame"
        );
    }

    #[test]
    fn relay_response_evicts_backend_when_failed_client_write_may_leave_unread_backend_bytes() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let relay_start = impl_src
            .find("pub(crate) async fn relay_response(")
            .expect("relay_response should exist");
        let relay_body = &impl_src[relay_start..];
        let write_error_start = relay_body
            .find("if let Err(err_write)")
            .expect("relay_response should handle client write failures");
        let write_error_body = &relay_body[write_error_start..];
        let write_error_end = write_error_body
            .find("return Err(err_write);")
            .expect("client write failure branch should stop the roundtrip");
        let write_error_body = &write_error_body[..write_error_end];

        assert!(
            relay_body.contains(
                "server.is_data_available() || server.is_async() || server.in_copy_mode()"
            ),
            "failed client writes must mark the backend bad when unread or async/COPY data may remain"
        );
        let mark_bad_idx = write_error_body
            .find("server.mark_bad(")
            .expect("failed client writes with unread data must mark the backend bad");
        if let Some(wait_idx) = write_error_body.find("server.wait_available().await") {
            assert!(
                mark_bad_idx < wait_idx,
                "failed client writes with unread backend data must mark bad before any drain"
            );
        }
        assert!(
            relay_body.contains("return Err(err_write);"),
            "failed client writes must stop the client roundtrip instead of continuing to pin a backend"
        );
    }

    #[test]
    fn relay_response_skips_drain_when_recv_error_already_marked_backend_bad() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let relay_start = impl_src
            .find("pub(crate) async fn relay_response(")
            .expect("relay_response should exist");
        let relay_body = &impl_src[relay_start..];
        let err_start = relay_body
            .find("Err(err) =>")
            .expect("relay_response should handle receive errors");
        let err_body = &relay_body[err_start..];
        let err_end = err_body
            .find("let mut msg = String::with_capacity(64);")
            .expect("receive-error log message should follow drain handling");
        let err_body = &err_body[..err_end];

        assert!(
            err_body.contains("if !server.is_bad()") && err_body.contains("server.wait_available().await"),
            "relay_response must not drain unread bytes after recv already marked the backend bad; \
             the bad backend will be evicted, and draining keeps sv_active pinned"
        );
    }
}

#[cfg(test)]
mod deallocate_fast_path_tests {
    use super::{
        response_contains_sql_prepare_command_complete, should_release_transaction_backend,
    };

    fn command_complete_frame(tag: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + tag.len());
        frame.push(b'C');
        frame.extend_from_slice(&(tag.len() as u32 + 4).to_be_bytes());
        frame.extend_from_slice(tag);
        frame
    }

    #[test]
    fn deallocate_fast_path_uses_strict_simple_query_body() {
        let src = include_str!("transaction.rs");
        let start = src
            .find("async fn try_handle_without_server")
            .expect("try_handle_without_server should exist");
        let body = &src[start..];
        let end = body
            .find("\n    /// Serve a `general.pooler_check_query`")
            .expect("pooler check handler should follow fast path");
        let body = &body[..end];

        assert!(
            body.contains("let query_bytes = simple_query_body(message);"),
            "DEALLOCATE fast path must validate Q framing via simple_query_body()"
        );
        assert!(
            !body.contains("&message[5..message.len() - 1]"),
            "DEALLOCATE fast path must not slice around a presumed null terminator"
        );
    }

    #[test]
    fn simple_deallocate_paths_do_not_cap_sql_with_long_comments() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let start = impl_src
            .find("async fn try_handle_without_server")
            .expect("try_handle_without_server should exist");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Serve a `general.pooler_check_query`")
            .expect("pooler check handler should follow fast path");
        let body = &body[..end];

        assert!(
            body.contains("extract_deallocate_target(simple_query_body(message))"),
            "DEALLOCATE paths must send all framed simple-query bodies through the strict SQL parser"
        );
        assert!(
            !body.contains("MAX_DEALLOCATE_MESSAGE_BYTES")
                && !body.contains("message.len() < MAX_DEALLOCATE_MESSAGE_BYTES")
                && !body.contains("message.len() >= MAX_DEALLOCATE_MESSAGE_BYTES"),
            "valid DEALLOCATE with long leading comments must not bypass cache invalidation by total frame length"
        );
    }

    /// pin the invariant that the synthetic
    /// `SIMPLE_DEALLOCATE_NAMED_ACK` (CommandComplete("DEALLOCATE") +
    /// ReadyForQuery('I')dle) can NEVER be emitted while a backend is
    /// checked out inside an open transaction - which would desync the
    /// client's transaction state (it would see 'I' where PostgreSQL
    /// would send 'T').
    ///
    /// The guarantee is *structural*, not a runtime guard:
    ///
    ///   1. `try_handle_without_server` takes NO `server` argument, so it
    ///      runs only on the idle path BEFORE any server is acquired. It
    ///      literally has no server whose transaction it could be inside.
    ///   2. Its sole invocation is gated by `client_pending_begin.is_none()`
    ///      and lives in the OUTER client loop. After a real statement
    ///      forces server checkout, every subsequent message - including a
    ///      mid-transaction simple-query `DEALLOCATE <name>` - is handled by
    ///      the INNER transaction loop, which dispatches 'Q' to
    ///      `handle_simple_query` (forwarded to the backend, real RFQ status
    ///      relayed) and never re-enters `try_handle_without_server`.
    ///
    /// Contrast the DISCARD-ALL fast path, which lives inside
    /// `handle_simple_query` (it DOES receive `&mut server`) and therefore
    /// genuinely needs an explicit `!server.in_transaction()` guard. The
    /// DEALLOCATE path needs no such guard precisely because it can only run
    /// when no server is held. If a future refactor moves the DEALLOCATE
    /// fast path into a server-holding context (or adds a `server` param to
    /// `try_handle_without_server`), this test fails and forces a re-think.
    #[test]
    fn deallocate_synthetic_ack_unreachable_while_server_held() {
        let src = include_str!("transaction.rs");

        // Restrict the structural checks to the real implementation,
        // excluding the `#[cfg(test)]` modules (whose source mentions the
        // same tokens and would otherwise self-trip the searches below).
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };

        // (1) The function that emits the synthetic 'I' ack must not take a
        //     Server: no server in scope => provably not inside a server
        //     transaction. We inspect the PARAMETER LIST only (from the `(`
        //     after the fn name to the closing `)` of the signature), so the
        //     `...without_server` name itself cannot match.
        let name_pos = impl_src
            .find("async fn try_handle_without_server(")
            .expect("try_handle_without_server should exist");
        let params_start = name_pos + "async fn try_handle_without_server".len();
        let params_end = params_start
            + impl_src[params_start..]
                .find(") -> Result<bool, Error> {")
                .expect("try_handle_without_server signature should terminate");
        let params = &impl_src[params_start..params_end];
        assert!(
            !params.contains("Server"),
            "try_handle_without_server must NOT take a Server parameter; a \
             server in scope here could be mid-transaction and the hardcoded \
             ReadyForQuery('I') would desync transaction state. params: {params:?}"
        );

        // (2) The synthetic ack is written from exactly one site in the impl,
        //     and that site sits inside the no-server fast path (between the
        //     fn signature and the next handler).
        let ack_writes = impl_src.matches("&SIMPLE_DEALLOCATE_NAMED_ACK").count();
        assert_eq!(
            ack_writes, 1,
            "the synthetic idle DEALLOCATE ack must be written from exactly \
             one site (the no-server fast path); a second site risks emitting \
             'I' while a server holds an open transaction"
        );
        let ack_pos = impl_src
            .find("&SIMPLE_DEALLOCATE_NAMED_ACK")
            .expect("synthetic ack write should exist");
        let fast_path_end = name_pos
            + impl_src[name_pos..]
                .find("\n    /// Serve a `general.pooler_check_query`")
                .expect("pooler check handler should follow the fast path");
        assert!(
            ack_pos > name_pos && ack_pos < fast_path_end,
            "the synthetic idle DEALLOCATE ack must be emitted only inside \
             try_handle_without_server (the no-server idle path)"
        );

        // (3) There is exactly one CALL site, it is gated on the idle path
        //     (client_pending_begin.is_none()), and it is NOT inside the
        //     inner transaction loop. The gate token sits immediately before
        //     the call.
        let call_sites = impl_src.matches(".try_handle_without_server(").count();
        assert_eq!(
            call_sites, 1,
            "try_handle_without_server must have exactly one call site (the \
             outer idle loop); a second call site could run with a server held"
        );
        let call_pos = impl_src
            .find(".try_handle_without_server(&message, current_pool, query_start_at)")
            .expect("the single call site should exist");
        let preamble = &impl_src[call_pos.saturating_sub(120)..call_pos];
        assert!(
            preamble.contains("self.client_pending_begin.is_none()"),
            "the try_handle_without_server call must be gated by \
             client_pending_begin.is_none() on the idle path"
        );
    }

    #[test]
    fn sql_prepare_pin_blocks_transaction_pool_release() {
        assert!(
            !should_release_transaction_backend(true, true, true),
            "transaction pooling must not return a backend that still owns \
             SQL-level PREPARE state for the current client"
        );
    }

    #[test]
    fn response_prepare_scan_matches_only_complete_command_complete() {
        let mut response = command_complete_frame(b"SELECT 1\0");
        response.extend_from_slice(&command_complete_frame(b"PREPARE\0"));

        assert!(response_contains_sql_prepare_command_complete(&response));

        let mut datarow = Vec::new();
        datarow.push(b'D');
        datarow.extend_from_slice(&(b"PREPARE\0".len() as u32 + 4).to_be_bytes());
        datarow.extend_from_slice(b"PREPARE\0");
        assert!(!response_contains_sql_prepare_command_complete(&datarow));

        let mut truncated = command_complete_frame(b"PREPARE\0");
        truncated.pop();
        assert!(!response_contains_sql_prepare_command_complete(&truncated));
    }

    #[test]
    fn sql_prepare_pin_disables_fast_release_before_relay() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let relay_start = impl_src
            .find("pub(crate) async fn relay_response(")
            .expect("relay_response should exist");
        let relay_body = &impl_src[relay_start..];
        assert!(
            relay_body.contains(
                "let can_fast_release = self.transaction_mode && !self.sql_prepare_session_pinned;"
            ),
            "fast-release buffering must be disabled while SQL PREPARE pins \
             the backend, otherwise the PREPARE response waits for a release \
             that the pin deliberately prevents"
        );

        let simple_start = impl_src
            .find("async fn handle_simple_query(")
            .expect("handle_simple_query should exist");
        let simple_body = &impl_src[simple_start..];
        let pin_pos = simple_body
            .find("self.sql_prepare_session_pinned = true;")
            .expect("SQL PREPARE should pin before relaying response");
        let relay_pos = simple_body
            .find("self.relay_response(server).await?;")
            .expect("simple-query relay call should exist");
        assert!(
            pin_pos < relay_pos,
            "SQL PREPARE pin must be armed before relay_response so the \
             response is flushed to the client instead of fast-release buffered"
        );
    }

    #[test]
    fn sql_prepare_pin_is_armed_from_relayed_command_complete() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let relay_start = impl_src
            .find("pub(crate) async fn relay_response(")
            .expect("relay_response should exist");
        let relay_body = &impl_src[relay_start..];

        let prepare_scan_pos = relay_body
            .find("response_contains_sql_prepare_command_complete(&response)")
            .expect("relay_response must detect late SQL PREPARE CommandComplete frames");
        let pin_pos = relay_body[prepare_scan_pos..]
            .find("self.sql_prepare_session_pinned = true;")
            .map(|offset| prepare_scan_pos + offset)
            .expect("late SQL PREPARE must pin before transaction fast-release");
        let fast_release_pos = relay_body
            .find(
                "let can_fast_release = self.transaction_mode && !self.sql_prepare_session_pinned;",
            )
            .expect("fast-release decision should be computed after late PREPARE detection");

        assert!(
            prepare_scan_pos < pin_pos && pin_pos < fast_release_pos,
            "relay_response must arm the SQL PREPARE pin before computing \
             fast-release eligibility for the response that contains PREPARE"
        );
    }

    #[test]
    fn server_held_simple_deallocate_updates_client_cache_before_forward() {
        let src = include_str!("transaction.rs");
        let impl_src = {
            let tests_start = src
                .find("\n#[cfg(test)]")
                .expect("at least one test module should follow the impl");
            &src[..tests_start]
        };
        let handle_start = impl_src
            .find("async fn handle_simple_query(")
            .expect("handle_simple_query should exist");
        let handle_body = &impl_src[handle_start..];
        let handle_end = handle_body
            .find("\n    /// Synthesize the wire response")
            .expect("discard response helper should follow handle_simple_query");
        let handle_body = &handle_body[..handle_end];

        let call = "self.track_forwarded_simple_deallocate_cache_state(message);";
        assert!(
            handle_body.contains(call),
            "server-held SimpleQuery path must invalidate client prepared cache for DEALLOCATE before forwarding"
        );
        let call_idx = handle_body.find(call).unwrap();
        let forward_idx = handle_body
            .find("self.execute_server_roundtrip(Some(message), server).await?")
            .expect("normal SimpleQuery forwarding call not found");
        assert!(
            call_idx < forward_idx,
            "DEALLOCATE cache invalidation must run before the query is forwarded"
        );

        let helper_start = impl_src
            .find("fn track_forwarded_simple_deallocate_cache_state(")
            .expect("DEALLOCATE cache invalidation helper should exist");
        let helper_body = &impl_src[helper_start..];
        let helper_end = helper_body
            .find("\n    /// Handle simple query")
            .expect("handle_simple_query should follow DEALLOCATE helper");
        let helper_body = &helper_body[..helper_end];
        assert!(
            helper_body.contains("extract_deallocate_target(simple_query_body(message))"),
            "helper must use the strict DEALLOCATE parser on the SimpleQuery body"
        );
        let discard_clear_call = ["self.prepared.", "discard_clear()"].concat();
        assert!(
            helper_body.contains(&discard_clear_call),
            "DEALLOCATE ALL must clear all client prepared state"
        );
        let named_pop_call = ["self.prepared.cache.", "pop(&key)"].concat();
        assert!(
            helper_body.contains(&named_pop_call),
            "DEALLOCATE <name> must remove the named client cache entry"
        );
    }
}

#[cfg(test)]
mod discard_response_tests {
    //! guard that the pre-computed DISCARD ALL
    //! responses stay byte-identical to what
    //! `command_complete("DISCARD ALL") + ready_for_query(in_tx)`
    //! produces. If the wire shape of CommandComplete or
    //! ReadyForQuery ever changes (e.g. a future protocol extension
    //! adds a field), this catches it before the static slice
    //! silently ships garbage to clients.
    use crate::messages::{command_complete, ready_for_query};

    fn rebuild_response_dynamic(in_transaction: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&command_complete("DISCARD ALL"));
        out.extend_from_slice(&ready_for_query(in_transaction));
        out
    }

    #[test]
    fn discard_all_response_idle_matches_dynamic() {
        let dynamic = rebuild_response_dynamic(false);
        // Mirror the static slice from `respond_to_simple_discard`.
        let pre_computed: &[u8] = &[
            b'C', 0, 0, 0, 16, b'D', b'I', b'S', b'C', b'A', b'R', b'D', b' ', b'A', b'L', b'L', 0,
            b'Z', 0, 0, 0, 5, b'I',
        ];
        assert_eq!(
            dynamic.as_slice(),
            pre_computed,
            "pre-computed DISCARD_ALL_RESPONSE_IDLE drifted from \
             command_complete + ready_for_query"
        );
    }

    #[test]
    fn discard_all_response_in_tx_matches_dynamic() {
        let dynamic = rebuild_response_dynamic(true);
        let pre_computed: &[u8] = &[
            b'C', 0, 0, 0, 16, b'D', b'I', b'S', b'C', b'A', b'R', b'D', b' ', b'A', b'L', b'L', 0,
            b'Z', 0, 0, 0, 5, b'T',
        ];
        assert_eq!(
            dynamic.as_slice(),
            pre_computed,
            "pre-computed DISCARD_ALL_RESPONSE_IN_TX drifted from \
             command_complete + ready_for_query"
        );
    }

    #[test]
    fn intercepted_discard_response_write_is_deadline_bound() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn respond_to_simple_discard")
            .expect("respond_to_simple_discard helper not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Handle Sync")
            .expect("handle_sync_flush should follow DISCARD response helper");
        let body = &body[..end];

        assert!(
            body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "intercepted DISCARD ALL client write must use proxy_copy_data_timeout"
        );
        assert!(
            body.contains("write_all_flush_timeout(&mut self.write, response"),
            "intercepted DISCARD ALL response must not wait forever on a slow client while holding a backend"
        );
        assert!(
            !body.contains("write_all_flush(&mut self.write, response"),
            "intercepted DISCARD ALL response must not use an unbounded client write"
        );
    }
}

#[cfg(test)]
mod v4_h2_deallocate_tests {
    use super::*;

    /// framing of the synthetic DEALLOCATE ack. When a client
    /// runs an EXTENDED-protocol `Parse "S_1"`, pg_doorman renames the
    /// statement to `DOORMAN_<n>` on the wire. A subsequent SIMPLE-query
    /// `DEALLOCATE S_1` cannot be forwarded verbatim - the backend only
    /// knows `DOORMAN_<n>` and would answer SQLSTATE 26000. For that case
    /// pg_doorman answers the client itself with this exact byte
    /// sequence: CommandComplete("DEALLOCATE") + ReadyForQuery(idle).
    #[test]
    fn simple_deallocate_named_ack_is_well_formed() {
        let ack = SIMPLE_DEALLOCATE_NAMED_ACK;
        // CommandComplete: 'C' + i32 len + "DEALLOCATE\0"
        assert_eq!(ack[0], b'C');
        let cc_len = i32::from_be_bytes([ack[1], ack[2], ack[3], ack[4]]);
        // len counts itself (4) + tag "DEALLOCATE\0" (11) = 15
        assert_eq!(cc_len, 15, "CommandComplete length field");
        assert_eq!(&ack[5..15], b"DEALLOCATE", "command tag");
        assert_eq!(ack[15], 0, "command tag NUL terminator");
        // ReadyForQuery: 'Z' + i32(5) + 'I'
        assert_eq!(ack[16], b'Z');
        let rfq_len = i32::from_be_bytes([ack[17], ack[18], ack[19], ack[20]]);
        assert_eq!(rfq_len, 5, "ReadyForQuery length field");
        assert_eq!(ack[21], b'I', "ReadyForQuery idle status");
        assert_eq!(ack.len(), 22, "total ack length");
    }

    /// the forward/synthesize decision for a `DEALLOCATE <name>`.
    ///
    /// - extended-renamed client statement (cache pop returned Some) with
    ///   prepared statements enabled -> SynthesizeAck (forwarding verbatim
    ///   would 26000 because the backend only knows DOORMAN_<n>).
    /// - unknown name / SQL-level PREPARE name (cache pop returned None)
    ///   -> Forward verbatim (preserves the F3 fix for
    ///   `PREPARE x; DEALLOCATE x; PREPARE x` 42P05; the backend knows x).
    /// - prepared statements disabled -> always Forward (pg_doorman never
    ///   renamed anything, so verbatim is correct).
    #[test]
    fn simple_deallocate_action_branches() {
        // extended-renamed name, gate on -> synthesize (avoid 26000)
        assert_eq!(
            simple_deallocate_action(true, true),
            DeallocateForwardAction::SynthesizeAck,
            "known extended-renamed client statement must not be forwarded verbatim"
        );
        // unknown / SQL-level name, gate on -> forward (preserve F3)
        assert_eq!(
            simple_deallocate_action(false, true),
            DeallocateForwardAction::Forward,
            "unknown/SQL-level name must forward verbatim to preserve F3"
        );
        // gate off -> always forward (nothing was renamed)
        assert_eq!(
            simple_deallocate_action(true, false),
            DeallocateForwardAction::Forward,
            "with prepared statements disabled nothing is renamed; forward verbatim"
        );
        assert_eq!(
            simple_deallocate_action(false, false),
            DeallocateForwardAction::Forward
        );
    }
}

#[cfg(test)]
mod v4_h3_function_call_tests {
    use super::*;

    /// a standalone FunctionCall ('F') is forwarded via a single
    /// server round-trip. F4 (1bdc1a0) added the forward but did not flush
    /// `self.buffer` first. If a client pipelined Parse/Bind (which append
    /// to `self.buffer` without round-tripping) and then sent 'F' before
    /// Sync, those buffered bytes are stranded and the next round-trip
    /// desyncs. The legit libpq lo_* path never pipelines, so the normal
    /// standalone-'F' (empty buffer) case must still forward; only the
    /// illegitimate non-empty-buffer sequence is rejected fail-closed.
    #[test]
    fn function_call_forwards_only_with_empty_buffer() {
        // Standalone 'F': empty buffer -> safe to forward.
        assert!(
            non_extended_protocol_can_forward(0, 0, 0),
            "a standalone FunctionCall with an empty pending buffer must forward"
        );
        // Pipelined Parse/Bind then 'F' before Sync: non-empty buffer ->
        // must NOT forward (would strand the buffered bytes -> desync).
        assert!(
            !non_extended_protocol_can_forward(7, 0, 0),
            "a FunctionCall preceded by buffered extended-protocol bytes must be rejected"
        );
        assert!(
            !non_extended_protocol_can_forward(1, 0, 0),
            "even one stranded byte must block the FunctionCall forward"
        );
    }

    #[test]
    fn non_extended_messages_reject_pending_synthetic_parse_metadata() {
        assert!(
            non_extended_protocol_can_forward(0, 0, 0),
            "standalone SimpleQuery/FunctionCall with no extended batch state must forward"
        );
        assert!(
            !non_extended_protocol_can_forward(0, 1, 0),
            "pending batch operation metadata must block non-extended forwarding"
        );
        assert!(
            !non_extended_protocol_can_forward(0, 0, 1),
            "pending skipped Parse metadata must block non-extended forwarding"
        );
        assert!(
            !non_extended_protocol_can_forward(7, 0, 0),
            "pending buffered extended bytes must still block non-extended forwarding"
        );
    }
}

#[cfg(test)]
mod app_name_set_discard_all_clears_pending_set_tests {
    #[test]
    fn discard_all_intercept_precedes_backend_checkout() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let fast_path_start = impl_src
            .find("async fn try_handle_without_server(")
            .expect("no-server fast path not found");
        let fast_path_end = fast_path_start
            + impl_src[fast_path_start..]
                .find("\n    fn track_forwarded_simple_deallocate_cache_state")
                .expect("no-server fast path end not found");
        let fast_path = &impl_src[fast_path_start..fast_path_end];
        assert!(
            fast_path.contains("contains_discard_all(simple_query_body(message))")
                && fast_path.contains("pool.settings.intercept_discard_all"),
            "standalone DISCARD ALL must be handled by the no-server path"
        );

        let simple_start = impl_src
            .find("async fn handle_simple_query(")
            .expect("simple-query handler not found");
        let simple_end = simple_start
            + impl_src[simple_start..]
                .find("\n    /// Synthesize the wire response")
                .expect("simple-query handler end not found");
        assert!(
            !impl_src[simple_start..simple_end].contains("contains_discard_all"),
            "server-held simple-query path must not duplicate DISCARD ALL interception"
        );

        let handle_start = impl_src
            .find("pub async fn handle(&mut self)")
            .expect("client handle loop not found");
        let handle = &impl_src[handle_start..];
        let intercept_call = handle
            .find(".try_handle_without_server(&message, current_pool, query_start_at)")
            .expect("no-server fast-path call not found");
        let checkout = handle
            .find("match current_pool.database.get().await")
            .expect("backend checkout not found");
        assert!(
            intercept_call < checkout,
            "DISCARD ALL interception must run before backend checkout"
        );
    }

    #[test]
    fn sync_params_plan_metric_records_checkout_and_consumption_paths() {
        let full = include_str!("transaction.rs");
        let src = full.split("#[cfg(test)]").next().unwrap_or(full);

        fn has_plan_path_call(src: &str, plan: &str, path: &str) -> bool {
            let plan = format!(r#""{plan}""#);
            let path = format!(r#""{path}""#);
            let mut offset = 0;
            while let Some(rel_idx) = src[offset..].find("inc_sync_params_plan(") {
                let start = offset + rel_idx;
                let Some(end_rel) = src[start..].find(");") else {
                    return false;
                };
                let call = &src[start..start + end_rel + 2];
                if call.contains(&plan) && call.contains(&path) {
                    return true;
                }
                offset = start + "inc_sync_params_plan(".len();
            }
            false
        }

        for (plan, path) in [
            ("empty", "none"),
            ("complex", "standalone"),
            ("app_name_only", "simple_query_piggyback"),
            ("app_name_only", "deferred_begin_preflush"),
            ("app_name_only", "non_simple_preflush"),
        ] {
            assert!(
                has_plan_path_call(src, plan, path),
                "sync-params plan/path metric is missing expected counter call: plan={plan}, path={path}"
            );
        }
    }

    #[test]
    fn simple_query_cleanup_attribution_uses_one_combined_scan() {
        let src = include_str!("transaction.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("async fn handle_simple_query(")
            .expect("simple-query handler not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Synthesize the wire response")
            .expect("simple-query handler end not found");
        let body = &body[..end];

        assert_eq!(
            body.matches("extract_set_and_reset_cleanup_commands(")
                .count(),
            1
        );
        assert!(!body.contains("extract_set_cleanup_commands("));
        assert!(!body.contains("extract_reset_cleanup_commands("));
    }

    /// Deferred-BEGIN ordering regression lock.
    ///
    /// `application_name` is a non-LOCAL GUC: a `SET application_name` issued
    /// INSIDE a transaction the client later `ROLLBACK`s is reverted by
    /// PostgreSQL, leaving a reused backend advertising the PREVIOUS service's
    /// name in `pg_stat_activity` (audit mis-attribution). Baseline
    /// `sync_parameters` ran the SET before the deferred BEGIN (outside any
    /// transaction). When a checkout deferred BOTH a `SET application_name`
    /// (`SyncPlan::AppNameOnly`) AND a client BEGIN, the deferred SET MUST be
    /// flushed BEFORE the BEGIN - never piggybacked onto the first query inside
    /// the transaction.
    ///
    /// Like the DISCARD-ALL guard above, this is a source-structure check (the
    /// real path needs a live backend; end-to-end behaviour is covered by the
    /// `setapp-piggyback.feature` `BEGIN; ...; ROLLBACK` BDD scenario). It fails
    /// fast if a refactor moves the SET flush after the BEGIN send.
    #[test]
    fn deferred_set_flushed_before_deferred_begin() {
        let src = include_str!("transaction.rs");
        let lines: Vec<&str> = src.lines().collect();

        // Locate the deferred-BEGIN block opener.
        let begin_guard_idx = lines
            .iter()
            .position(|l| l.contains("if let Some(begin_msg) = pending_begin {"))
            .expect("deferred-BEGIN block (`if let Some(begin_msg) = pending_begin`) not found");

        // The SET-application_name flush must consume the pending slot...
        let set_take_rel = lines[begin_guard_idx..]
            .iter()
            .position(|l| l.contains("if let Some(set_sql) = self.pending_app_name_set.take()"))
            .expect(
                "deferred-BEGIN block must flush a deferred SET application_name \
                 (`self.pending_app_name_set.take()`) before sending the BEGIN",
            );
        let set_take_idx = begin_guard_idx + set_take_rel;

        // ...strictly BEFORE the BEGIN is sent to the backend.
        let begin_send_rel = lines[begin_guard_idx..]
            .iter()
            .position(|l| l.contains("send_and_flush_timeout(&begin_msg"))
            .expect("deferred-BEGIN block has no `send_and_flush_timeout(&begin_msg, ...)` send");
        let begin_send_idx = begin_guard_idx + begin_send_rel;

        assert!(
            set_take_idx < begin_send_idx,
            "deferred SET application_name (line {}) must be flushed BEFORE the \
             deferred BEGIN send (line {}); otherwise the SET runs inside the \
             transaction and a client ROLLBACK reverts application_name, \
             corrupting audit attribution on the reused backend",
            set_take_idx + 1,
            begin_send_idx + 1,
        );
    }

    /// A piggybacked internal `SET application_name` is drained through the
    /// normal backend receive path before the client query response is relayed.
    /// Client SET/RESET cleanup attribution must therefore be queued after that
    /// internal drain, otherwise the internal `CommandComplete("SET")` can
    /// consume the first client attribution entry and hide a later
    /// `SET ROLE` / `SET SESSION AUTHORIZATION`.
    #[test]
    fn piggyback_tracks_client_cleanup_after_internal_set_swallow() {
        let src = include_str!("transaction.rs");
        let lines: Vec<&str> = src.lines().collect();

        let piggy_idx = lines
            .iter()
            .position(|l| l.contains("if let Some(set_sql) = self.pending_app_name_set.take()"))
            .expect("piggyback SET application_name branch not found");
        let swallow_idx = piggy_idx
            + lines[piggy_idx..]
                .iter()
                .position(|l| l.contains("swallow_set_response().await?"))
                .expect("piggyback branch must swallow the internal SET response");
        let track_set_idx = piggy_idx
            + lines[piggy_idx..]
                .iter()
                .position(|l| l.contains("server.track_set_cleanup_commands(set_cleanup_commands)"))
                .expect("piggyback branch must queue client SET cleanup attribution");
        let track_reset_idx = piggy_idx
            + lines[piggy_idx..]
                .iter()
                .position(|l| {
                    l.contains("server.track_reset_cleanup_commands(reset_cleanup_commands)")
                })
                .expect("piggyback branch must queue client RESET cleanup attribution");
        let relay_idx = piggy_idx
            + lines[piggy_idx..]
                .iter()
                .position(|l| l.contains("self.relay_response(server).await?"))
                .expect("piggyback branch must relay the client response");

        assert!(
            swallow_idx < track_set_idx && track_set_idx < relay_idx,
            "client SET cleanup attribution must be queued after internal \
             SET application_name swallow (line {}) and before client relay \
             (line {}), got line {}",
            swallow_idx + 1,
            relay_idx + 1,
            track_set_idx + 1,
        );
        assert!(
            swallow_idx < track_reset_idx && track_reset_idx < relay_idx,
            "client RESET cleanup attribution must be queued after internal \
             SET application_name swallow (line {}) and before client relay \
             (line {}), got line {}",
            swallow_idx + 1,
            relay_idx + 1,
            track_reset_idx + 1,
        );
    }

    #[test]
    fn app_name_only_sync_clears_internal_set_cleanup_state() {
        let src = include_str!("transaction.rs");
        let lines: Vec<&str> = src.lines().collect();

        let piggy_idx = lines
            .iter()
            .position(|l| l.contains("if let Some(set_sql) = self.pending_app_name_set.take()"))
            .expect("piggyback SET application_name branch not found");
        let swallow_idx = piggy_idx
            + lines[piggy_idx..]
                .iter()
                .position(|l| l.contains("swallow_set_response().await?"))
                .expect("piggyback branch must swallow internal SET response");
        let track_set_idx = piggy_idx
            + lines[piggy_idx..]
                .iter()
                .position(|l| l.contains("server.track_set_cleanup_commands(set_cleanup_commands)"))
                .expect("piggyback branch must track client SET cleanup commands");
        assert!(
            lines[swallow_idx..track_set_idx]
                .iter()
                .any(|l| l.contains("server.clear_internal_set_cleanup_state();")),
            "piggybacked AppNameOnly SET must clear its internal SET cleanup \
             state before client cleanup attribution is queued"
        );

        let begin_idx = lines
            .iter()
            .position(|l| l.contains("if let Some(begin_msg) = pending_begin {"))
            .expect("deferred-BEGIN block not found");
        let begin_set_idx = begin_idx
            + lines[begin_idx..]
                .iter()
                .position(|l| l.contains("if let Some(set_sql) = self.pending_app_name_set.take()"))
                .expect("deferred-BEGIN block must flush pending SET");
        let begin_small_query_idx = begin_set_idx
            + lines[begin_set_idx..]
                .iter()
                .position(|l| l.contains("server.small_simple_query(&set_sql).await"))
                .expect("deferred-BEGIN pending SET must use small_simple_query");
        let begin_send_idx = begin_idx
            + lines[begin_idx..]
                .iter()
                .position(|l| l.contains("send_and_flush_timeout(&begin_msg"))
                .expect("deferred BEGIN send not found");
        assert!(
            lines[begin_small_query_idx..begin_send_idx]
                .iter()
                .any(|l| l.contains("server.clear_internal_set_cleanup_state();")),
            "deferred-BEGIN AppNameOnly SET preflush must clear its internal \
             SET cleanup state before BEGIN is sent"
        );

        let edge_idx = lines
            .iter()
            .position(|l| {
                l.contains("Deferred-SET preflush: a non-simple-query first message after checkout")
            })
            .expect("deferred-SET preflush branch not found");
        let edge_small_query_idx = edge_idx
            + lines[edge_idx..]
                .iter()
                .position(|l| l.contains("server.small_simple_query(&set_sql).await"))
                .expect("deferred-SET preflush must use small_simple_query");
        let edge_end_idx = edge_idx
            + lines[edge_idx..]
                .iter()
                .position(|l| l.contains("Only mark the backend bad"))
                .expect("deferred-SET preflush block end marker not found");
        assert!(
            lines[edge_small_query_idx..edge_end_idx]
                .iter()
                .any(|l| l.contains("server.clear_internal_set_cleanup_state();")),
            "Deferred-SET AppNameOnly preflush must clear its internal SET \
             cleanup state before backend-bound client messages are processed"
        );
    }

    /// cancel-routing regression lock.
    ///
    /// After `server.claim()` installs the (connection_id, secret_key)
    /// cancel-routing entry, every checkout-prep / deferred-SET backend round-trip
    /// must call `release_after_inner_handler_error()` before propagating an
    /// error. A bare `.await?` / unguarded `return Err` orphans that row until
    /// `Client::Drop`, the  window where a parallel CancelRequest can fire
    /// at a recycled backend pid. Source-structure check (the real path needs a
    /// live backend; the helper is behaviourally covered by
    /// `inner_handler_error_releases_cancel_mapping_before_client_drop`).
    #[test]
    fn checkout_prep_roundtrips_release_cancel_routing_on_error() {
        let full = include_str!("transaction.rs");
        // Inspect only production code (everything before the first test
        // module); otherwise the needle string literals below match
        // themselves inside this very test.
        let src = full.split("#[cfg(test)]").next().unwrap_or(full);

        // The prep / deferred-SET round-trips must not propagate with a bare
        // `?` / `.await?`, which skips the cancel-routing cleanup.
        for needle in [
            "server.compute_sync_plan(&self.server_parameters)?",
            "server.sync_parameters(&self.server_parameters).await?",
            "server.sync_parameter_diff(parameter_diff).await?",
            "server.small_simple_query(&set_sql).await?",
        ] {
            assert!(
                !src.contains(needle),
                "found orphan-prone bare `{needle}`; wrap it and call \
                 release_after_inner_handler_error() on the error path"
            );
        }

        // Both deferred-BEGIN error returns must release first.
        let lines: Vec<&str> = src.lines().collect();
        let begin_idx = lines
            .iter()
            .position(|l| l.contains("if let Some(begin_msg) = pending_begin {"))
            .expect("deferred-BEGIN block not found");
        let send_marker = lines[begin_idx..]
            .iter()
            .position(|l| l.contains("server.send_and_flush(&begin_msg"))
            .map(|i| begin_idx + i)
            .expect("deferred-BEGIN send not found");
        let block_end = lines[send_marker..]
            .iter()
            .position(|l| l.contains("query_start_at = now();"))
            .map(|i| send_marker + i)
            .expect("end of deferred-BEGIN block not found");
        for (i, line) in lines[send_marker..block_end].iter().enumerate() {
            if line.trim() == "return Err(err);" {
                let abs = send_marker + i;
                let guarded = lines[abs.saturating_sub(3)..abs]
                    .iter()
                    .any(|l| l.contains("release_after_inner_handler_error()"));
                assert!(
                    guarded,
                    "deferred-BEGIN `return Err(err);` at line {} must release the \
                     cancel-routing entry first",
                    abs + 1
                );
            }
        }
    }

    /// cancel-quarantine regression lock
    /// - defense layer 2.
    ///
    /// Cross-client cancel safety rests on the checkout loop EVICTING (mark_bad + continue) a
    /// backend whose pid is quarantined in `CANCELED_PIDS` - not merely
    /// consuming the marker and handing the backend to the client. If a refactor
    /// turns this into a bare consume without the `Fresh => mark_bad + continue`
    /// branch, a forwarded cancel could land on the next client's in-flight
    /// query. Source-structure check (the real path needs a live pool; the
    /// concurrency invariant cannot be reproduced deterministically).
    #[test]
    fn cancel_quarantine_checkout_evicts_marked_backend() {
        let full = include_str!("transaction.rs");
        // Inspect production code only (test literals below would self-match).
        let src = full.split("#[cfg(test)]").next().unwrap_or(full);
        let lines: Vec<&str> = src.lines().collect();

        let idx = lines
            .iter()
            .position(|l| l.contains("canceled_pids_consume(conn.get_process_id())"))
            .expect("checkout-loop cancel-quarantine consume not found");
        let window: String = lines[idx..(idx + 8).min(lines.len())].join("\n");
        assert!(
            window.contains("CancelMarker::Fresh"),
            "the checkout must branch on a FRESH cancel marker"
        );
        assert!(
            window.contains("mark_bad"),
            "a FRESH-quarantined pid at checkout must mark_bad the backend, not reuse it"
        );
        assert!(
            window.contains("continue"),
            "a FRESH-quarantined pid at checkout must `continue` to another backend"
        );
    }

    /// cancel-quarantine regression lock
    /// - defense layer 1 (load-bearing).
    ///
    /// In `handle_cancel_mode` the quarantine marker (`should_forward_cancel`,
    /// which inserts into `CANCELED_PIDS`) MUST be set while the
    /// `client_server_map` `Ref` guard for the victim key is still held: that
    /// shard read-lock blocks the victim's `release()` `remove()` until the
    /// marker is set, guaranteeing marker-insert happens-before the backend can
    /// return to the pool and be checked out by another client. The async
    /// `Server::cancel` MUST come AFTER (Ref dropped at the end of the match
    /// arm). If a refactor moves `should_forward_cancel` after the lookup's Ref
    /// is released, the marker could be set after another client already checked
    /// out the backend - reopening the cancel-quarantine race.
    #[test]
    fn cancel_marker_set_before_async_cancel_under_map_ref() {
        let full = include_str!("transaction.rs");
        let src = full.split("#[cfg(test)]").next().unwrap_or(full);
        let lines: Vec<&str> = src.lines().collect();

        let fn_idx = lines
            .iter()
            .position(|l| l.contains("async fn handle_cancel_mode"))
            .expect("handle_cancel_mode not found");
        let end_rel = lines[fn_idx + 1..]
            .iter()
            .position(|l| l.trim_start().starts_with("async fn "))
            .map(|i| fn_idx + 1 + i)
            .unwrap_or(lines.len());
        let body = &lines[fn_idx..end_rel];

        let get_rel = body
            .iter()
            .position(|l| l.contains(".get(&(self.connection_id"))
            .expect("victim-key client_server_map lookup not found in handle_cancel_mode");
        let marker_rel = body
            .iter()
            .position(|l| l.contains("should_forward_cancel(t.process_id"))
            .expect("should_forward_cancel call not found in handle_cancel_mode");
        let cancel_rel = body
            .iter()
            .position(|l| l.contains("Server::cancel("))
            .expect("Server::cancel not found in handle_cancel_mode");

        assert!(
            get_rel < marker_rel,
            "should_forward_cancel must run AFTER the client_server_map lookup (inside the \
             Ref-guarded match arm), so the marker is inserted under the shard read-lock"
        );
        assert!(
            marker_rel < cancel_rel,
            "the quarantine marker (should_forward_cancel) must be set BEFORE the async \
             Server::cancel is awaited"
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
mod relay_response_client_write_failure_tests {
    use super::*;
    use crate::client::buffer_pool::PooledBuffer;
    use crate::client::core::PreparedStatementState;
    use crate::pool::PoolIdentifier;
    use crate::server::ServerParameters;
    use crate::stats::ClientStats;
    use dashmap::DashMap;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::Context;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, ReadBuf};

    /// Client read half that never yields data or EOF, modelling a live
    /// client that stays silent for the duration of the exchange. An
    /// `Empty` reader is unsuitable here: its instant EOF makes
    /// `recv_server_response_or_client_disconnect` treat the client as
    /// disconnected before the backend socket readiness is delivered.
    struct SilentReader;

    impl tokio::io::AsyncRead for SilentReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Pending
        }
    }

    /// Client write half that fails every write with `BrokenPipe`,
    /// modelling a client socket after an RST arrived.
    struct BrokenPipeWriter;

    impl tokio::io::AsyncWrite for BrokenPipeWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "client connection reset",
            )))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "client connection reset",
            )))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_client_with_broken_pipe_writer() -> Client<SilentReader, BrokenPipeWriter> {
        let addr = "127.0.0.1:6543".parse().unwrap();
        Client {
            read: BufReader::new(SilentReader),
            write: BrokenPipeWriter,
            buffer: PooledBuffer::new(),
            addr,
            addr_str: addr.to_string(),
            read_buf: BytesMut::new(),
            connection_id: 1,
            cancel_mode: false,
            transaction_mode: false,
            sql_prepare_session_pinned: false,
            secret_key: 0,
            client_server_map: Arc::new(DashMap::new()),
            stats: Arc::new(ClientStats::default()),
            admin: false,
            last_server_stats: None,
            connected_to_server: false,
            session_xact_start: None,
            pool_name: "db".to_string(),
            username: "user".to_string(),
            cached_pool_id: PoolIdentifier::new("db", "user"),
            migration_pool: None,
            migration_pool_is_dynamic: false,
            server_parameters: ServerParameters::default(),
            prepared: PreparedStatementState::new(true, 0),
            max_memory_usage: u64::MAX,
            client_last_messages_in_tx: PooledBuffer::new(),
            client_pending_begin: None,
            pending_app_name_set: None,
            #[cfg(unix)]
            raw_fd: None,
            #[cfg(all(unix, feature = "tls-migration"))]
            ssl_ptr: None,
        }
    }

    const COMMAND_COMPLETE_SELECT_1_READY_FOR_QUERY_IDLE: &[u8] = &[
        b'C', 0, 0, 0, 13, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1', 0, b'Z', 0, 0, 0, 5,
        b'I',
    ];

    #[tokio::test]
    async fn client_write_failure_after_ready_for_query_runs_release_cleanup() {
        let mut client = test_client_with_broken_pipe_writer();
        let (mut server, mut peer) = crate::server::Server::test_silent_socket();
        server.set_release_query(Some("SELECT pg_advisory_unlock_all()"));
        server.arm_release_cleanup();

        // The backend already produced the complete response for the
        // in-flight client query: CommandComplete + ReadyForQuery(I).
        peer.write_all(COMMAND_COMPLETE_SELECT_1_READY_FOR_QUERY_IDLE)
            .await
            .expect("peer must write the buffered backend response");

        // Serve the release-cleanup Query triggered by the failed client
        // write. The timeout only bounds the failure mode where the
        // cleanup is never sent to the backend.
        let cleanup_peer = tokio::spawn(async move {
            let mut header = [0_u8; 5];
            let read =
                tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut header)).await;
            let Ok(Ok(_)) = read else {
                return false;
            };
            assert_eq!(header[0], b'Q');
            let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; (len - 4) as usize];
            peer.read_exact(&mut body)
                .await
                .expect("peer must receive the cleanup Query body");
            assert_eq!(&body, b"SELECT pg_advisory_unlock_all();\0");
            peer.write_all(COMMAND_COMPLETE_SELECT_1_READY_FOR_QUERY_IDLE)
                .await
                .expect("peer must confirm the cleanup with ReadyForQuery(I)");
            true
        });

        let result = client.relay_response(&mut server).await;
        let cleanup_served = cleanup_peer.await.expect("cleanup peer task must finish");

        assert!(
            result.is_err(),
            "the original client write error must be propagated"
        );
        assert!(
            cleanup_served,
            "the backend must receive the release cleanup query after the client write failure"
        );
        assert!(
            !server.is_bad(),
            "a backend that finished its response and its release cleanup stays healthy"
        );
        assert!(
            !server.test_release_cleanup_pending(),
            "the confirmed release round trip must disarm the pending flag"
        );
    }
}
