// Implementation of the PostgreSQL server (database) protocol.

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::string::ToString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use once_cell::sync::Lazy;
use tokio::sync::Notify;

/// counter for in-flight graceful Terminate tasks
/// spawned by `Server::drop`. Drained by
/// `wait_terminate_tasks_drained` before `process::exit(0)` to keep
/// the graceful idle-session shutdown fix working at scale (RELOAD storm +
/// SIGUSR2 with hundreds of backends).
pub(crate) static IN_FLIGHT_TERMINATE_TASKS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static IN_FLIGHT_TERMINATE_DRAINED: Lazy<Notify> = Lazy::new(Notify::new);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsyncExpectedResponse {
    Operation,
    Describe,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SetResponseOutcome {
    Applied,
    Rejected { sqlstate: String, message: String },
}

/// Wait (with bounded timeout) for all in-flight graceful Terminate
/// tasks to finish. Returns the number of tasks still running when
/// the timeout fires.
///
/// register the `Notified` future BEFORE the
/// counter load. Pre-fix order was load -> notified().await - between
/// those two points another task could `fetch_sub` to 0 and
/// `notify_waiters()`; the wake landed on zero registered waiters,
/// and the caller then registered and slept until the full timeout
/// fired. Now `notified()` is created at the top of each iteration
/// (registers on first poll); the recheck happens AFTER it's pinned
/// so no wake can be missed.
pub async fn wait_terminate_tasks_drained(timeout: Duration) -> usize {
    let _ = tokio::time::timeout(timeout, async {
        loop {
            let notify_fut = IN_FLIGHT_TERMINATE_DRAINED.notified();
            tokio::pin!(notify_fut);
            // Enable the listener BEFORE the load so a wake that
            // happens between the load and `.await` is captured.
            notify_fut.as_mut().enable();
            if IN_FLIGHT_TERMINATE_TASKS.load(Ordering::SeqCst) == 0 {
                return;
            }
            notify_fut.await;
        }
    })
    .await;
    IN_FLIGHT_TERMINATE_TASKS.load(Ordering::SeqCst)
}

struct TerminateTaskGuard<'a> {
    counter: &'a AtomicUsize,
    drained: &'a Notify,
}

impl<'a> TerminateTaskGuard<'a> {
    fn new(counter: &'a AtomicUsize, drained: &'a Notify) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter, drained }
    }
}

impl Drop for TerminateTaskGuard<'_> {
    fn drop(&mut self) {
        let remaining = self
            .counter
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        if remaining == 0 {
            self.drained.notify_waiters();
        }
    }
}

use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::{debug, error, info, warn};
use lru::LruCache;
use tokio::io::{AsyncReadExt, AsyncWrite, BufStream};

use crate::auth::scram_client::ScramSha256;
use crate::config::{get_config, tls, Address, BackendAuthMethod, User};
use crate::errors::{Error, ServerIdentifier};
use crate::messages::PgErrorMsg;
use crate::messages::{
    read_message_data_with_memory_limit, simple_query, startup, sync, BytesMutReader, Close, Parse,
};
use crate::pool::{CancelTarget, ClientServerMap, CANCELED_PIDS};
use crate::stats::ServerStats;

use super::authentication::handle_authentication;
use super::cleanup::{CleanupState, PendingCleanupDisarms, ResetCleanupCommand, SetCleanupCommand};
use super::parameters::ServerParameters;
use super::stream::{create_tcp_stream_inner, create_unix_stream_inner, StreamInner};
use super::{prepared_statements, protocol_io, startup_cancel};

/// Buffer flush threshold in bytes (8 KiB).
/// When the buffer reaches this size, it will be flushed to avoid excessive memory usage.
const BUFFER_FLUSH_THRESHOLD: usize = 8192;

/// TCP buffered-stream capacity in
/// bytes. The default was tokio's 8 KiB. On the bulk-
/// response scenario (1M-row `SELECT * FROM pgbench_accounts` via
/// pg_doorman) `BufStream::poll_read` alone consumed 5.85% of CPU
/// because the 8 KiB internal buffer was refilled on every COPY-row
/// boundary; the `__memcpy_generic` syscall->buffer copy added
/// another 2.17%. 64 KiB matches typical Linux TCP send/recv buffer
/// per-iteration capacity and shrinks the refill rate by 8×.
///
/// Applied symmetrically to `BufStream` on backend connections and
/// `BufReader` on client connections so neither side bottlenecks the
/// pair. Memory cost: 64 KiB × (one backend per pool slot + one
/// client per connection). On a max-1024-client / 256-backend pool
/// that is ~80 MiB total, acceptable on production hosts that
/// already size buffer pools with `BUFFER_FLUSH_THRESHOLD = 8 KiB`
/// times a few-hundred queue slots.
pub const BUF_STREAM_CAPACITY: usize = 65536;

/// Deadline for the detached graceful Terminate finisher spawned from
/// `Server::drop`. The task is best-effort cleanup; it must not retain the
/// backend fd and upgrade drain counter forever if the socket stops making
/// write progress.
const GRACEFUL_TERMINATE_TASK_TIMEOUT: Duration = Duration::from_secs(1);

/// Hard deadline for `small_simple_query`. Housekeeping queries (`ROLLBACK`,
/// `RESET ALL`, `DEALLOCATE ALL`, the per-pool `release_query`) must never
/// block a checkin path indefinitely: if the backend stops responding within
/// this window we mark it bad and let the pool replace it. Borrowed from the
/// iServ patch, which derived the value from observed checkin latencies on
/// production traffic.
pub(crate) const HOUSEKEEPING_TIMEOUT: Duration = Duration::from_secs(30);

/// Historical iServ default for the per-checkin release query. Releases
/// session-scoped state that PostgreSQL does not clear between transactions:
/// advisory locks plus any session variables stored by the `pg_variables`
/// extension. Used when `release_query` is omitted from the pool config; if
/// `pgv_free()` is not available on the target database the operator must
/// either install `pg_variables` or set `release_query = ""` to disable.
const RELEASE_SESSION_QUERY: &str =
    "SELECT pg_catalog.pg_advisory_unlock_all(), public.pgv_free();";

async fn finish_graceful_terminate<W>(
    mut stream: W,
    bytes: BytesMut,
    bytes_written: usize,
    timeout: Duration,
) -> bool
where
    W: AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    tokio::time::timeout(timeout, async {
        // write only the suffix that wasn't already accepted by
        // the kernel.
        if bytes_written < bytes.len() {
            let _ = stream.write_all(&bytes[bytes_written..]).await;
        }
        let _ = stream.flush().await;
        let _ = stream.shutdown().await;
    })
    .await
    .is_ok()
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedReleaseQuery {
    sql: Arc<str>,
    frame: Bytes,
}

impl ResolvedReleaseQuery {
    #[inline]
    pub(crate) fn sql(&self) -> &str {
        &self.sql
    }

    #[inline]
    pub(crate) fn frame(&self) -> &[u8] {
        &self.frame
    }
}

impl std::ops::Deref for ResolvedReleaseQuery {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.sql()
    }
}

/// Translate a pool-level `release_query` config value into the SQL and
/// pre-encoded simple-query frame used on every check-in:
///
/// * `None` => iServ-compatible default (`RELEASE_SESSION_QUERY`).
/// * `Some("")` => disabled, nothing runs at checkin.
/// * `Some(custom)` => exactly the operator-provided SQL.
///
/// The SQL and frame use reference-counted storage so every backend in the
/// pool can reuse them without rebuilding or copying either representation.
pub(crate) fn resolve_release_query(configured: Option<&str>) -> Option<ResolvedReleaseQuery> {
    let sql: Arc<str> = match configured {
        None => Arc::from(RELEASE_SESSION_QUERY),
        Some("") => return None,
        Some(query) => Arc::from(query),
    };
    let mut wire_sql = String::with_capacity(sql.len() + 1);
    wire_sql.push_str(&sql);
    wire_sql.push(';');
    let frame = simple_query(&wire_sql).freeze();
    Some(ResolvedReleaseQuery { sql, frame })
}

fn combined_sql_preview(combined: &str, max_bytes: usize) -> String {
    let preview = crate::utils::strings::truncate_bytes(combined, max_bytes);
    if preview.len() == combined.len() {
        preview.to_string()
    } else {
        format!("{}…(+{}B)", preview, combined.len() - preview.len())
    }
}

/// Wrap a GUC value in a PostgreSQL dollar-quoted string literal that is safe
/// to splice into `SET key TO <literal>`. Single-quote escaping (`''`) breaks
/// for backend-controlled values containing apostrophes inside SCRAM salts,
/// `application_name` strings with quotes, search_path entries with embedded
/// quotes, and so on - dollar-quoting is the only delimiter PostgreSQL
/// guarantees never to interpret.
///
/// Picks the shortest `$pgdoorman<N>$` tag that produces a syntactically valid
/// dollar-quoted literal for `value`. Naive `value.contains(&tag)` is **not**
/// sufficient: a value that ends with `$pgdoorman<N>` (no trailing `$`) creates
/// a boundary-overlap with the closing tag - concatenating `tag + value + tag`
/// produces a premature `$pgdoorman<N>$` match inside the body, PostgreSQL closes
/// the literal early, and the remaining bytes parse as garbage tokens
/// (SQLSTATE 42601). With `sync_parameters` being called for every checked-out
/// backend on every per-client startup, an attacker who controls
/// `application_name = "x$pgdoorman0"` can amplify into a DoS that marks every
/// backend bad on each cycle.
///
/// To eliminate the boundary-overlap class of bugs the loop validates the
/// constructed `candidate` directly: the tag must appear **exactly** twice,
/// at positions `0` and `tag.len() + value.len()` (the wrap positions of
/// `format!("{tag}{value}{tag}")`). Any other match signature means the tag
/// is unsafe for this value and the search advances.
pub(crate) fn sql_string_literal(value: &str) -> Result<String, Error> {
    // Defensive cap: a benign value finds a free tag on the first
    // iteration (n=0). The realistic adversarial worst case is "value
    // contains $pgdoorman0$ through $pgdoorman<K>$ as substrings AND
    // ends with $pgdoorman<K+1> as suffix-overlap" - which requires a
    // value of at least 12 × K bytes. The 4096-byte release/prewarm
    // validators cap K at ~340; for `application_name` (no operator
    // cap) the same value would have to be ~6 KB. A cap of 4096
    // iterations is generously above either, and is reached only if
    // someone deliberately crafts a value of that shape.
    //
    // Fail closed if no safe dollar-quote tag can be found. The caller's
    // `mark_bad` / error path runs and the backend is evicted instead of
    // executing SQL with an ambiguous delimiter.
    const TAG_BUMP_CAP: u32 = 4096;
    let mut n: u32 = 0;
    while n < TAG_BUMP_CAP {
        let tag = format!("$pgdoorman{n}$");
        let candidate = format!("{tag}{value}{tag}");
        let expected_close = tag.len() + value.len();
        let positions: Vec<usize> = candidate.match_indices(&tag).map(|(i, _)| i).collect();
        if positions == [0, expected_close] {
            return Ok(candidate);
        }
        n += 1;
    }
    Err(Error::ProtocolSyncError(format!(
        "GUC value rejected: no safe dollar-quote tag found within \
         {TAG_BUMP_CAP} candidates (value length {} bytes) - falling \
         back to attacker-predictable tag would risk SQL injection",
        value.len()
    )))
}

/// Represents a connection to a PostgreSQL server (backend).
///
/// This structure maintains the state of a single connection to a PostgreSQL database server,
/// including connection details, transaction state, buffering, and statistics.
/// The connection can be reused across multiple client sessions through connection pooling.
#[derive(Debug)]
pub struct Server {
    /// Server address configuration including host, port, database, username, and role (primary/replica).
    pub(crate) address: Address,

    /// Buffered TCP or Unix socket stream for communication with the PostgreSQL server.
    ///
    /// Wrapped in `ManuallyDrop` so the `Drop` impl can take ownership of
    /// the underlying tokio stream and hand it to a short-lived task that
    /// performs a proper async `Terminate + flush + shutdown` (see graceful shutdown path).
    /// Outside of `Drop`, the value is always live; `Deref`/`DerefMut`
    /// autoderef makes the wrapping transparent at call sites.
    pub(crate) stream: std::mem::ManuallyDrop<BufStream<StreamInner>>,

    /// Response buffer for accumulating server messages before forwarding them to the client.
    pub(crate) buffer: BytesMut,

    /// Reusable read buffer for message parsing. Avoids heap allocation per message —
    /// clear()+reserve() reuses existing capacity. Cleared on checkin for defence-in-depth.
    pub(crate) read_buf: BytesMut,

    /// Server runtime parameters received during startup (e.g., client_encoding, TimeZone, DateStyle).
    /// These parameters are tracked and synchronized with clients to maintain session consistency.
    pub(crate) server_parameters: ServerParameters,

    /// PostgreSQL backend process ID, used for query cancellation requests.
    process_id: i32,

    /// Secret key associated with the backend process, required for query cancellation.
    secret_key: i32,

    /// Transaction state: true if the server is currently inside a transaction block.
    pub(crate) in_transaction: bool,

    /// Transaction state inferred from CommandComplete tags within the current
    /// response stream before ReadyForQuery arrives. ReadyForQuery remains the
    /// authoritative backend state, but cleanup disarms must fail closed while a
    /// same-batch transaction can still roll back the reset command.
    pub(crate) command_complete_in_transaction: bool,

    /// Indicates whether more data is available from the server to be read.
    /// Set to false when ReadyForQuery message is received.
    pub(crate) data_available: bool,

    /// COPY mode state: true when the server is in COPY IN or COPY OUT mode.
    /// In this mode, data transfer follows a different protocol.
    pub(crate) in_copy_mode: bool,

    /// Async mode state: true when using Flush messages instead of Sync.
    /// In async mode, the server doesn't wait for ReadyForQuery after each command.
    async_mode: bool,

    /// Number of expected responses in async mode.
    /// Decremented when receiving terminating messages (CommandComplete, BindComplete, etc.).
    /// When reaches 0, we know all expected responses have been received.
    expected_responses: u32,

    /// Ordered terminal response kinds expected in async mode.
    /// RowDescription/NoData only consume Describe slots; Execute SELECT
    /// starts with RowDescription but consumes its Operation slot at
    /// CommandComplete/PortalSuspended/EmptyQuery.
    expected_response_sequence: VecDeque<AsyncExpectedResponse>,

    /// Connection health flag: true if the connection is broken and should be removed from the pool.
    /// Set to true on protocol errors, I/O errors, or unexpected server behavior.
    pub(crate) bad: bool,

    /// True after pg_doorman has sent an internal server-only round trip
    /// whose responses have not been fully drained yet. If the owning
    /// future is cancelled or panics in that window, unread backend frames
    /// remain on the socket and the connection must not be reused.
    internal_round_trip_in_flight: bool,

    /// Tracks whether the connection needs cleanup (RESET ALL, DEALLOCATE ALL, CLOSE ALL)
    /// before being returned to the pool. Set when SET, PREPARE, or DECLARE statements are executed.
    pub(crate) cleanup_state: CleanupState,

    /// Client-side attribution for upcoming `CommandComplete("SET")` tags.
    /// PostgreSQL reports ordinary GUC `SET`, `SET ROLE`, and
    /// `SET SESSION AUTHORIZATION` as `SET`, so simple-query forwarding records
    /// the statement class before the response path updates cleanup state.
    pub(crate) pending_set_cleanup_commands: VecDeque<SetCleanupCommand>,

    /// Client-side attribution for upcoming `CommandComplete("RESET")` tags.
    /// PostgreSQL reports both `RESET ALL` and per-GUC `RESET ...` as `RESET`,
    /// so simple-query forwarding records only the reset statements, in order,
    /// before the response path decides whether a tag may disarm SET cleanup.
    pub(crate) pending_reset_cleanup_commands: VecDeque<ResetCleanupCommand>,

    /// Cleanup disarms observed in the current implicit transaction. They are
    /// committed only by an error-free idle ReadyForQuery.
    pub(crate) pending_cleanup_disarms: PendingCleanupDisarms,

    /// Whether the response cycle since the previous ReadyForQuery contained
    /// an ErrorResponse and therefore rolled back implicit transaction effects.
    pub(crate) response_cycle_had_error: bool,

    /// Shared mapping of client-to-server connections for query cancellation support.
    /// Allows canceling queries by mapping client process IDs to server process IDs.
    client_server_map: ClientServerMap,

    /// Timestamp when this connection was established to the server.
    connected_at: chrono::naive::NaiveDateTime,

    /// Statistics collector for this server connection (bytes sent/received, queries executed, etc.).
    pub stats: Arc<ServerStats>,

    /// Application name of the client currently using this server connection.
    /// Updated when the connection is checked out from the pool.
    application_name: String,

    /// Timestamp of the last successful I/O operation (send or receive).
    /// Used to detect idle connections and implement connection timeouts.
    pub last_activity: SystemTime,

    /// monotonic counterpart of `last_activity` used as
    /// the throttle gate inside `touch_activity()`. quanta's
    /// `Instant::now()` reads the TSC (x86_64) or CNTVCT_EL0 (aarch64)
    /// without a syscall, so the gate itself is essentially free -
    /// orders of magnitude cheaper than `SystemTime::now()`, which
    /// does enter the kernel on each call. Updated lock-step with
    /// `last_activity` so callers can sample either without races.
    pub(crate) last_activity_quanta: quanta::Instant,

    /// Configuration flag: if true, execute cleanup statements (RESET ALL, etc.) on dirty connections
    /// before returning them to the pool. If false, discard dirty connections instead.
    cleanup_connections: bool,

    /// Configuration flag: if true, log when server parameters change for debugging purposes.
    pub(crate) log_client_parameter_status_changes: bool,

    /// LRU cache of prepared statement names currently registered on this server connection.
    /// When the cache is full, evicted statements are automatically closed on the server.
    /// None if prepared statement caching is disabled.
    ///
    /// ahash hasher (see `prepared_statements::has` /
    /// `prepared_statements::add_to_cache` for rationale).
    pub(crate) prepared_statement_cache: Option<LruCache<String, (), ahash::RandomState>>,

    /// Queue of prepared statement names currently being registered on the server.
    /// Used to track Parse messages that haven't been confirmed yet.
    pub(crate) registering_prepared_statement: VecDeque<String>,

    /// Prepared statement names whose optimistic server/client cache entries
    /// must be rolled back because PostgreSQL returned ErrorResponse before
    /// ParseComplete.
    pub(crate) rejected_prepared_statement_names: Vec<String>,

    /// True when prepared statements were added to the LRU cache via
    /// register_prepared_statement(should_send_parse_to_server=false) but
    /// the client buffer has not yet been flushed to PostgreSQL (Sync/Flush
    /// not received). If the client disconnects before flushing, checkin_cleanup
    /// uses this flag to trigger DEALLOCATE ALL and clear the stale cache.
    pub(crate) has_pending_cache_entries: bool,

    /// Statements evicted from the server LRU during the current batch but
    /// whose Close has NOT yet been sent to PostgreSQL. The statements still
    /// exist on PostgreSQL — Close is deferred until Sync/Flush completes so
    /// that any Bind referencing them in the client buffer succeeds first.
    /// was `Vec<String>` - `has_prepared_statement` did
    /// O(N) linear scan on every Bind/Parse-rewrite. Under cache
    /// pressure (LRU at capacity) the queue grows to dozens of names
    /// and the per-batch cost balloons to O(N²). HashSet gives O(1)
    /// membership; the `drain()` semantics for `send_deferred_eviction_closes`
    /// are equivalent (the wire order of Close frames doesn't matter -
    /// each Close is independent).
    pub(crate) deferred_eviction_closes: std::collections::HashSet<String>,

    /// Cancel requests must use the same transport as the main connection.
    connected_with_tls: bool,

    /// Session mode flag: true when the pool operates in session mode.
    /// In session mode, PostgreSQL ErrorResponse in async mode does not mark connection as bad,
    /// because the connection remains valid and the client can continue using it.
    pub(crate) session_mode: bool,

    /// Maximum message size (in bytes) before switching to streaming mode for large backend messages.
    /// Messages larger than this threshold are streamed directly to avoid excessive memory usage.
    /// A value of 0 disables streaming.
    pub(crate) max_message_size: i32,

    /// Large message header saved when recv() needs to return accumulated buffer first.
    /// The large DataRow/CopyData/FunctionCallResponse will be streamed on the next recv() call.
    pub(crate) pending_large_message: Option<(u8, i32)>,

    /// Reason for closing this connection, set before dropping.
    /// Used by Drop to produce a single log line with cause and effect.
    pub(crate) close_reason: Option<String>,

    /// Per-connection lifetime override (ms). Set on fallback connections so
    /// they expire before the local backend recovers.
    pub(crate) override_lifetime_ms: Option<u64>,

    /// GUC names injected by configured `startup_parameters` for this backend.
    /// Checkout sync must not overwrite them with client StartupMessage
    /// values. Shared because every backend from the same pool uses the
    /// same set.
    operator_managed_startup_keys: Arc<HashSet<String>>,

    /// Most recent PostgreSQL ErrorResponse for the current backend
    /// exchange. `small_simple_query` uses it to return SQL failures as
    /// `Err`, so callers do not mirror rejected SET/RESET operations
    /// into the backend snapshot.
    pub(crate) last_sql_error: Option<(String, String)>,

    /// Resolved release query shared by every backend in this pool. It carries
    /// both the configured SQL and its pre-encoded release-only Query frame.
    release_query: Option<ResolvedReleaseQuery>,

    /// True while the current checkout still owes a successful
    /// `release_query` round trip. Armed by [`Server::arm_release_cleanup`]
    /// on every checkout when `release_query` is configured; cleared only
    /// after `send_checkin_cleanup` confirms the release statement finished
    /// with a clean ReadyForQuery. `Object::drop` refuses to recycle a
    /// backend with this flag set, so a checkout that never reached
    /// `finalize_checkin` (client RST mid-response, panic, cancellation)
    /// closes the backend instead of leaking session-local state (advisory
    /// locks, pg_variables) into the idle pool. Not part of `is_bad()`: an
    /// active backend with the flag set is healthy, it just has not passed
    /// its checkin cleanup yet.
    release_cleanup_pending: bool,

    /// Whether the DISCARD ALL synthetic-response fast path is allowed for
    /// this backend. Mirrors `Pool.intercept_discard_all`. Installed by
    /// `ServerPool::create` right after startup; queried from
    /// `Client::handle_simple_query` to decide whether to short-circuit
    /// the simple query or forward it to PostgreSQL. Defaulting to `true`
    /// here keeps the iServ contract intact for the `Server::startup`
    /// construction path that does not go through the pool builder
    /// (e.g. ad-hoc admin probes, tests).
    intercept_discard_all: bool,
}

impl std::fmt::Display for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "[{}]-{}@{}:{}/{}/{}",
            self.process_id,
            self.address.username,
            self.address.host,
            self.address.port,
            self.address.database,
            self.application_name
        )
    }
}

/// Outcome of classifying the checkout parameter sync WITHOUT touching the
/// wire. Produced by [`Server::compute_sync_plan`] / [`classify_sync_plan`].
///
/// The classifier mirrors [`Server::sync_parameters`]' diff computation
/// exactly (same `compare_params`, same `operator_managed_startup_keys`
/// retain, same `sql_string_literal` quoting) so the `AppNameOnly` SET text
/// is byte-identical to what `sync_parameters` would emit - minus the
/// trailing `;`, which only exists in `sync_parameters` because it
/// concatenates multiple statements into one simple-query string.
#[derive(Debug)]
pub(crate) enum SyncPlan {
    /// Diff empty - no SET/RESET needed.
    Empty,
    /// Only `application_name` differs - ready-to-send standalone SET
    /// (no trailing `;`).
    AppNameOnly(String),
    /// Anything else (search_path, TimeZone, RESET, multiple keys) - caller
    /// must fall back to the proven `sync_parameters()` round-trip.
    Complex,
}

/// Pure core of [`Server::compute_sync_plan`], factored out so it can be unit
/// tested without constructing a full [`Server`]. Reads nothing but the three
/// inputs it is handed.
///
/// `backend_parameters` is the server snapshot (the `self.server_parameters`
/// side), `operator_managed_startup_keys` is the operator override set, and
/// `incoming_parameters` is the client's desired state.
fn classify_sync_plan(
    backend_parameters: &ServerParameters,
    operator_managed_startup_keys: &HashSet<String>,
    incoming_parameters: &ServerParameters,
) -> Result<SyncPlan, Error> {
    let mut diff = backend_parameters.compare_params(incoming_parameters);

    // Configured startup_parameters win over client StartupMessage values -
    // identical retain to sync_parameters.
    if !operator_managed_startup_keys.is_empty() {
        diff.retain(|k, _| !operator_managed_startup_keys.contains(k));
    }

    if diff.is_empty() {
        return Ok(SyncPlan::Empty);
    }

    // AppNameOnly iff exactly one entry, key `application_name`, action SetTo.
    if diff.len() == 1 {
        if let Some(crate::server::parameters::ParamAction::SetTo(value)) =
            diff.get("application_name")
        {
            // Same quoting sync_parameters uses, so the SET text matches
            // byte-for-byte (minus the trailing ';').
            let literal = sql_string_literal(value)?;
            return Ok(SyncPlan::AppNameOnly(format!(
                "SET application_name TO {literal}"
            )));
        }
    }

    Ok(SyncPlan::Complex)
}

impl Server {
    /// Classify what parameter sync this checkout needs WITHOUT sending
    /// anything on the wire. Pure thin wrapper over [`classify_sync_plan`];
    /// mirrors [`Server::sync_parameters`]' diff computation exactly so the
    /// `AppNameOnly` SET text is byte-identical to what `sync_parameters`
    /// would emit (minus the trailing `;`).
    pub(crate) fn compute_sync_plan(
        &self,
        parameters: &ServerParameters,
    ) -> Result<SyncPlan, Error> {
        classify_sync_plan(
            &self.server_parameters,
            &self.operator_managed_startup_keys,
            parameters,
        )
    }

    /// Execute an arbitrary query against the server.
    /// It will use the simple query protocol.
    /// Result will not be returned, so this is useful for things like `SET` or `ROLLBACK`.
    ///
    /// Uses a single `HOUSEKEEPING_TIMEOUT` deadline that covers both the
    /// send and the full recv loop, so a backend that stops making forward
    /// progress mid-exchange (instead of hanging only on the first read)
    /// cannot wedge the checkin path forever. On timeout the backend is
    /// marked bad and the error is returned to the caller so the pool drops
    /// the connection.
    pub async fn small_simple_query(&mut self, query: &str) -> Result<(), Error> {
        let query = simple_query(query);
        self.small_simple_query_frame(&query).await
    }

    async fn small_simple_query_frame(&mut self, query: &[u8]) -> Result<(), Error> {
        // Reset SQL-error capture for this round trip before reading a
        // new ReadyForQuery.
        self.last_sql_error = None;

        let deadline = tokio::time::Instant::now() + HOUSEKEEPING_TIMEOUT;
        self.begin_internal_round_trip();

        match tokio::time::timeout_at(deadline, self.send_and_flush(query)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                self.mark_bad("housekeeping send timeout in small_simple_query");
                return Err(Error::SocketError(
                    "timeout sending housekeeping query".to_string(),
                ));
            }
        }

        let mut noop = tokio::io::sink();
        loop {
            match tokio::time::timeout_at(deadline, self.recv(&mut noop, None)).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    // returned `Err(err)` WITHOUT
                    // `mark_bad`. The caller's `?` propagates but the
                    // misleading comment in `pool/inner.rs:1599` claims
                    // "check_alive already called mark_bad" - false for
                    // transport errors. If `small_simple_query` is invoked
                    // outside the evict-then-drop path (it is - by
                    // `finalize_checkin` and `sync_parameters`) the caller
                    // could put a `bad=false` Server back into the pool.
                    self.mark_bad(&format!(
                        "transport error in small_simple_query recv: {err}"
                    ));
                    return Err(err);
                }
                Err(_) => {
                    self.mark_bad("housekeeping recv timeout in small_simple_query");
                    return Err(Error::SocketError(
                        "timeout waiting for response to housekeeping query".to_string(),
                    ));
                }
            }

            if !self.data_available {
                break;
            }
        }

        self.finish_internal_round_trip();

        // Transport success is not SQL success: ErrorResponse is captured
        // by the read loop and surfaced here.
        if let Some((sqlstate, message)) = self.last_sql_error.take() {
            return Err(Error::QueryError(format!(
                "backend rejected query (SQLSTATE {sqlstate}): {message}"
            )));
        }

        Ok(())
    }

    /// Consume exactly one simple-query response (CommandComplete + ReadyForQuery)
    /// the caller has ALREADY flushed to the backend, discarding it. Used by the
    /// piggyback path (`handle_simple_query`) after sending a SET
    /// application_name concatenated with the client's first frame. Mirrors
    /// `small_simple_query`'s recv loop and ErrorResponse handling (same
    /// `recv`, same `data_available` guard, same `last_sql_error` surfacing)
    /// but WITHOUT sending.
    ///
    /// The recv loop is bounded by a single `HOUSEKEEPING_TIMEOUT` deadline
    /// (same as `small_simple_query`). Without it, a wedged backend that sent
    /// `CommandComplete` but never the `ReadyForQuery` would block the client
    /// task forever (we are draining the SET's response before relaying the
    /// client's own query). On timeout the backend is marked bad and a
    /// `SocketError` is returned so the pool drops the connection. A complete
    /// SQL-level rejection is returned separately because the following
    /// client query is still valid and already executing on the same stream.
    pub(crate) async fn swallow_set_response(&mut self) -> Result<SetResponseOutcome, Error> {
        self.last_sql_error = None;
        let deadline = tokio::time::Instant::now() + HOUSEKEEPING_TIMEOUT;
        self.begin_internal_round_trip();
        let mut noop = tokio::io::sink();
        loop {
            match tokio::time::timeout_at(deadline, self.recv(&mut noop, None)).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    self.mark_bad(&format!(
                        "transport error in swallow_set_response recv: {err}"
                    ));
                    self.finish_internal_round_trip();
                    return Err(err);
                }
                Err(_) => {
                    self.mark_bad("swallow_set_response recv timeout");
                    self.finish_internal_round_trip();
                    return Err(Error::SocketError(
                        "timeout waiting for response to piggybacked SET application_name"
                            .to_string(),
                    ));
                }
            }
            if !self.data_available {
                break;
            }
        }
        self.finish_internal_round_trip();

        // Transport success is not SQL success: ErrorResponse is captured by
        // the read loop, but it does not desynchronize the pipelined client
        // query that follows this ReadyForQuery.
        if let Some((sqlstate, message)) = self.last_sql_error.take() {
            return Ok(SetResponseOutcome::Rejected { sqlstate, message });
        }
        Ok(SetResponseOutcome::Applied)
    }

    /// Check if the connection is alive by sending a minimal query (`;`).
    /// The provided `timeout` bounds the **entire** send + recv exchange via a
    /// single deadline, so a backend that ACKs the send but then stops reading
    /// can no longer wedge the caller. On timeout the backend is marked bad.
    /// Returns Ok(()) if connection is alive, Err if dead or timeout exceeded.
    pub async fn check_alive(&mut self, timeout: Duration) -> Result<(), Error> {
        let query = simple_query(";");
        let deadline = tokio::time::Instant::now() + timeout;

        // Match `small_simple_query`: reset SQL-error capture before sending
        // so any ErrorResponse received during this round trip can be cleanly
        // attributed to it, and a stale error from a prior path doesn't leak
        // into the next housekeeping caller's post-recv inspection of
        // `last_sql_error`.
        self.last_sql_error = None;

        match tokio::time::timeout_at(deadline, self.send_and_flush(&query)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                self.mark_bad("alive check send timeout");
                return Err(Error::SocketError(
                    "timeout sending alive check query".to_string(),
                ));
            }
        }

        let mut noop = tokio::io::sink();
        loop {
            match tokio::time::timeout_at(deadline, self.recv(&mut noop, None)).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    // mark_bad before propagating so
                    // callers don't return a transport-broken Server
                    // to the pool. Mirrors the timeout branch below.
                    self.mark_bad(&format!("transport error in check_alive recv: {err}"));
                    return Err(err);
                }
                Err(_) => {
                    self.mark_bad("alive check recv timeout");
                    return Err(Error::SocketError(
                        "timeout waiting for alive check response".to_string(),
                    ));
                }
            }

            if !self.data_available {
                break;
            }
        }

        Ok(())
    }

    /// Returns the PostgreSQL backend process ID for this connection.
    /// Used for query cancellation and connection tracking.
    #[inline(always)]
    pub fn get_process_id(&self) -> i32 {
        self.process_id
    }

    /// Returns a copy of all server parameters as a HashMap.
    /// Includes runtime parameters like client_encoding, TimeZone, DateStyle, etc.
    #[inline(always)]
    pub fn server_parameters_as_hashmap(&self) -> HashMap<String, String> {
        self.server_parameters.as_hashmap()
    }

    /// Receive data from the server in response to a client request.
    /// This method must be called multiple times while `self.is_data_available()` is true
    /// in order to receive all data the server has to offer.
    pub async fn recv<C>(
        &mut self,
        client_stream: C,
        client_server_parameters: Option<&mut ServerParameters>,
    ) -> Result<BytesMut, Error>
    where
        C: tokio::io::AsyncWrite + std::marker::Unpin,
    {
        protocol_io::recv(self, client_stream, client_server_parameters).await
    }

    /// Indicate that this server connection cannot be re-used and must be discarded.
    pub fn mark_bad(&mut self, reason: &str) {
        error!(
            "[{}@{}] server marked bad pid={}: {reason}",
            self.address.username, self.address.pool_name, self.process_id
        );
        self.bad = true;
    }

    pub(crate) fn begin_internal_round_trip(&mut self) {
        self.internal_round_trip_in_flight = true;
    }

    pub(crate) fn finish_internal_round_trip(&mut self) {
        self.internal_round_trip_in_flight = false;
    }

    /// throttled liveness-timestamp refresh for the
    /// hot send/recv path. `SystemTime::now()` is a syscall
    /// (`clock_gettime(CLOCK_REALTIME)` via the vdso path) - at
    /// ~210K calls/sec/worker (every protocol_io send/recv fires it,
    /// six sites total) it accounted for `arch_counter_get_cntvct`
    /// 0.93% in the profiling and the matching vdso frames.
    ///
    /// The downstream consumer is the zombie-backend scan in
    /// `Pool::evict_dead_backends`, which only needs the answer
    /// "did this backend see I/O within the last 30 s?" Sub-second
    /// precision is wasted on it. Throttle the writes to one per
    /// 100 ms using the cheap `quanta::Instant` clock (TSC /
    /// CNTVCT_EL0, no syscall): the inner gate costs a few ns;
    /// the SystemTime write only fires on ~1% of invocations.
    ///
    /// Semantics preserved: zombie scan compares
    /// `SystemTime::elapsed()` against 30 s - a worst-case 100 ms
    /// lag in `last_activity` is invisible against that threshold.
    #[inline(always)]
    pub fn touch_activity(&mut self) {
        const THROTTLE: Duration = Duration::from_millis(100);
        let now_q = quanta::Instant::now();
        if now_q.saturating_duration_since(self.last_activity_quanta) < THROTTLE {
            return;
        }
        self.last_activity_quanta = now_q;
        self.last_activity = SystemTime::now();
    }

    /// Returns a future that completes when the server socket becomes readable.
    /// Between queries in a transaction, BufStream is empty (everything was read
    /// up to ReadyForQuery), so readable on the underlying socket correctly
    /// reflects new data from the server (e.g., FATAL after idle_in_transaction_session_timeout).
    ///
    /// Raw-socket readiness is blind to bytes already pulled into the
    /// BufStream userspace buffer. Only use this where the read buffer is
    /// known to be empty; when a response may already be buffered (pipelined
    /// replies such as the piggybacked SET + first client query), use
    /// [`Self::wait_server_data`] instead or the wait deadlocks.
    pub async fn server_readable(&self) {
        let _ = self.stream.get_ref().readable().await;
    }

    /// Completes when backend response bytes are available to `recv()`:
    /// either already buffered inside the BufStream (e.g. a second pipelined
    /// response that arrived in the same kernel read as the previous one) or
    /// newly readable on the socket. Cancel-safe: `fill_buf` only peeks, no
    /// bytes are consumed until `recv()` reads them.
    ///
    /// The piggyback path sends two `Q` frames in one flush; after the SET
    /// response is swallowed, the client query's response is frequently
    /// already sitting in the BufStream buffer. A raw-socket readiness wait
    /// never fires for those bytes, deadlocking the relay until the client
    /// disconnects.
    pub async fn wait_server_data(&mut self) {
        use tokio::io::AsyncBufReadExt;
        let _ = self.stream.fill_buf().await;
    }

    /// Verify that server_readable() readiness is genuine, not spurious.
    /// Returns true if the connection is alive (WouldBlock = no real data).
    /// Returns false if the server sent data or closed the connection (dead).
    pub fn check_server_alive(&self) -> bool {
        if self.stream.get_ref().is_tls() {
            // For TLS connections, readable() fires on raw TCP socket readiness.
            // Calling try_read() on the raw socket would consume bytes that the
            // TLS layer hasn't processed, corrupting the session.
            //
            // On an idle PostgreSQL connection, the raw socket should never become
            // readable (PostgreSQL does not send unsolicited data, and TLS
            // renegotiation is disabled since PG14). If readable() fired, the
            // server disconnected or sent an error — treat as dead.
            return false;
        }
        let mut buf = [0u8; 1];
        matches!(
            self.stream.get_ref().try_read(&mut buf),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        )
    }

    /// Server & client are out of sync, we must discard this connection.
    /// This happens with clients that misbehave.
    pub fn is_bad(&self) -> bool {
        self.bad || self.internal_round_trip_in_flight
    }

    /// Drains any remaining data from the server that hasn't been read yet.
    /// Used to synchronize connection state when data is unexpectedly available.
    /// All received data is discarded (sent to a sink).
    pub async fn wait_available(&mut self) {
        self.wait_available_with_deadline(HOUSEKEEPING_TIMEOUT)
            .await
    }

    /// Inner body of [`wait_available`] with the drain deadline injected so
    /// unit tests can drive it with a short timeout instead of the 30s
    /// production value.
    async fn wait_available_with_deadline(&mut self, drain_timeout: Duration) {
        if !self.is_data_available() {
            self.stats.wait_idle();
            return;
        }
        warn!(
            "[{}@{}] draining unread data from server pid={}",
            self.address.username, self.address.pool_name, self.process_id
        );
        self.begin_internal_round_trip();
        // bound the whole drain with a
        // single deadline, exactly like small_simple_query / check_alive /
        // swallow_set_response. Without it, a backend that stalls between
        // messages on a live-but-silent socket blocks recv forever, pinning
        // the per-connection task and leaking the checked-out backend out of
        // the pool (TCP keepalive cannot detect an app-level stall, and a
        // checked-out backend is unreachable to the background dead-backend
        // eviction). On expiry the backend is marked bad so it is evicted.
        let deadline = tokio::time::Instant::now() + drain_timeout;
        loop {
            if !self.is_data_available() {
                self.stats.wait_idle();
                break;
            }
            self.stats.wait_reading();
            match tokio::time::timeout_at(deadline, self.recv(&mut tokio::io::sink(), None)).await {
                Ok(Ok(_)) => self.stats.wait_idle(),
                Ok(Err(err_read_response)) => {
                    error!(
                        "[{}@{}] server read error pid={}: {err_read_response}",
                        self.address.username, self.address.pool_name, self.process_id
                    );
                    self.mark_bad(&format!(
                        "wait_available failed to drain unread server data: {err_read_response}"
                    ));
                    break;
                }
                Err(_) => {
                    error!(
                        "[{}@{}] timed out draining unread server data pid={}",
                        self.address.username, self.address.pool_name, self.process_id
                    );
                    self.mark_bad("wait_available timed out draining unread server data");
                    break;
                }
            }
        }
        self.finish_internal_round_trip();
    }

    /// Returns true if the server is in async mode (using Flush instead of Sync).
    /// In async mode, the server doesn't send ReadyForQuery after each command.
    #[inline(always)]
    pub fn is_async(&self) -> bool {
        self.async_mode
    }

    pub async fn send_and_flush_timeout(
        &mut self,
        messages: &[u8],
        duration: Duration,
    ) -> Result<(), Error> {
        protocol_io::send_and_flush_timeout(self, messages, duration).await
    }

    pub async fn send_and_flush(&mut self, messages: &[u8]) -> Result<(), Error> {
        protocol_io::send_and_flush(self, messages).await
    }

    /// If the server is still inside a transaction.
    /// If the client disconnects while the server is in a transaction, we will clean it up.
    #[inline(always)]
    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    /// Returns true if the server is currently in COPY mode (COPY IN or COPY OUT).
    /// In COPY mode, data transfer follows a different protocol than normal queries.
    #[inline(always)]
    pub fn in_copy_mode(&self) -> bool {
        self.in_copy_mode
    }

    /// Return why this backend must not be recycled synchronously from
    /// `Object::drop`.
    ///
    /// Normal transaction checkin can run async cleanup (`ROLLBACK`, `RESET`,
    /// `DEALLOCATE`, release_query) before reuse. `Drop` cannot await that
    /// cleanup, so any state that would require it must fail closed and let
    /// RAII close the PostgreSQL socket instead of returning the backend to
    /// another client.
    pub(crate) fn recycle_safety_violation(&self) -> Option<&'static str> {
        if self.pending_large_message.is_some() {
            return Some("returned with pending large frame");
        }
        if self.in_copy_mode() {
            return Some("returned in copy-mode");
        }
        if self.is_data_available() {
            return Some("returned with data available");
        }
        if self.is_async() {
            return Some("returned in async protocol mode");
        }
        if !self.buffer.is_empty() {
            return Some("returned with not-empty buffer");
        }
        if self.internal_round_trip_in_flight {
            return Some("returned with internal round trip in flight");
        }
        if self.in_transaction() {
            return Some("returned in transaction");
        }
        if self.cleanup_state.needs_cleanup() {
            return Some("returned with dirty session state");
        }
        if self.has_pending_cache_entries {
            return Some("returned with pending prepared cache entries");
        }
        if !self.deferred_eviction_closes.is_empty() {
            return Some("returned with deferred prepared-statement closes");
        }
        if !self.pending_reset_cleanup_commands.is_empty() {
            return Some("returned with pending reset attribution");
        }
        if !self.pending_set_cleanup_commands.is_empty() {
            return Some("returned with pending set attribution");
        }
        // Checked last so the more specific COPY / transaction / unread-data
        // / protocol-state reasons above keep their diagnostics. An armed
        // flag on an otherwise clean backend means the checkout ended
        // without a confirmed release-query round trip (client RST after
        // ReadyForQuery, panic, cancellation), so session-local state such
        // as advisory locks or pg_variables may still be present.
        if self.release_cleanup_pending {
            return Some("returned without successful release cleanup");
        }
        None
    }

    /// Returns a string representation of the server address (host:port/database@user).
    #[inline(always)]
    pub fn address_to_string(&self) -> String {
        self.address.to_string()
    }

    /// Perform any necessary cleanup before putting the server
    /// connection back in the pool
    pub async fn checkin_cleanup(&mut self) -> Result<(), Error> {
        let stmts = self.collect_checkin_cleanup_sqls()?;
        self.send_checkin_cleanup(stmts, None).await
    }

    fn track_internal_checkin_cleanup_attribution(&mut self, stmts: &[String]) {
        for stmt in stmts {
            match stmt.as_str() {
                "RESET ALL" => self.track_reset_cleanup_commands([ResetCleanupCommand::ResetAll]),
                "RESET ROLE" => self.track_reset_cleanup_commands([ResetCleanupCommand::ResetRole]),
                "RESET SESSION AUTHORIZATION" => self
                    .track_reset_cleanup_commands([ResetCleanupCommand::ResetSessionAuthorization]),
                _ => {}
            }
        }
    }

    /// Validate the connection state and return the list of housekeeping
    /// SQL statements that have to be executed before the backend can be
    /// reused. Returns `Err` (and `mark_bad`s the connection) for any
    /// protocol-desync precondition.
    ///
    /// Side effects:
    ///   * sets `cleanup_state.needs_cleanup_prepare = true` if the
    ///     prepared-statement cache is in an inconsistent state with
    ///     the server (pending cache entries or deferred eviction
    ///     closes)
    ///   * drains `deferred_eviction_closes`
    ///
    /// Does NOT touch the network. The returned statements are
    /// ordered: ROLLBACK first (if applicable), RESET ROLE, then any
    /// dirty-state resets. `finalize_checkin` appends the per-pool
    /// `release_query` to this list before sending so the entire
    /// checkin path is one network round-trip instead of two or three.
    fn collect_checkin_cleanup_sqls(&mut self) -> Result<Vec<String>, Error> {
        if let Some((code, len)) = self.pending_large_message {
            warn!(
                "[{}@{}] server returned with pending large frame pid={} type={} len={}",
                self.address.username, self.address.pool_name, self.process_id, code as char, len
            );
            self.mark_bad("returned with pending large frame");
            return Err(Error::ProtocolSyncError(format!(
                "Protocol synchronization error: Server {} (database: {}, user: {}) was returned to the pool after a large-frame header was read but before the body was drained.",
                self.address.host, self.address.database, self.address.username
            )));
        }
        if self.in_copy_mode() {
            warn!(
                "[{}@{}] server returned in copy-mode pid={}",
                self.address.username, self.address.pool_name, self.process_id
            );
            self.mark_bad("returned in copy-mode");
            return Err(Error::ProtocolSyncError(format!(
                "Protocol synchronization error: Server {} (database: {}, user: {}) was returned to the pool while still in COPY mode. This may indicate a client disconnected during a COPY operation.",
                self.address.host, self.address.database, self.address.username
            )));
        }
        if self.is_data_available() {
            warn!(
                "[{}@{}] server returned with data available pid={}",
                self.address.username, self.address.pool_name, self.process_id
            );
            self.mark_bad("returned with data available");
            return Err(Error::ProtocolSyncError(format!(
                "Protocol synchronization error: Server {} (database: {}, user: {}) was returned to the pool while still having data available. This may indicate a client disconnected before receiving all query results.",
                self.address.host, self.address.database, self.address.username
            )));
        }
        if self.is_async() {
            warn!(
                "[{}@{}] server returned in async protocol mode pid={} expected_responses={}",
                self.address.username,
                self.address.pool_name,
                self.process_id,
                self.expected_responses()
            );
            self.mark_bad("returned in async protocol mode");
            return Err(Error::ProtocolSyncError(format!(
                "Protocol synchronization error: Server {} (database: {}, user: {}) was returned to the pool while still in async Flush mode. ReadyForQuery has not reconciled backend transaction state.",
                self.address.host, self.address.database, self.address.username
            )));
        }
        if !self.buffer.is_empty() {
            warn!(
                "[{}@{}] server returned with non-empty buffer pid={}",
                self.address.username, self.address.pool_name, self.process_id
            );
            self.mark_bad("returned with not-empty buffer");
            return Err(Error::ProtocolSyncError(format!(
                "Protocol synchronization error: Server {} (database: {}, user: {}) was returned to the pool with a non-empty buffer. This may indicate a client disconnected before the server response was fully processed.",
                self.address.host, self.address.database, self.address.username
            )));
        }

        // Promote pending cache mismatches to a DEALLOCATE ALL reset.
        if self.has_pending_cache_entries {
            self.cleanup_state.needs_cleanup_prepare = true;
            self.has_pending_cache_entries = false;
        }
        if !self.deferred_eviction_closes.is_empty() {
            self.cleanup_state.needs_cleanup_prepare = true;
            self.deferred_eviction_closes.clear();
        }

        // The prepared-statement cache on this backend is in an
        // inconsistent state with the client's view (either a Parse
        // succeeded server-side but the client cache forgot it, or the
        // client cache has entries that were never flushed to the
        // server) AND the operator has disabled cleanup queries on
        // checkin. We cannot issue `DEALLOCATE ALL` to bring the two
        // sides back in sync, and silently leaving the mismatch in
        // place would let the next client Parse on a deterministic
        // `DOORMAN_N` name hit SQLSTATE 42P05 ("prepared statement
        // already exists"). Mark the backend bad so the pool drops it
        // instead of reusing it in a corrupted state.
        //
        // This was a latent bug in the upstream design - the previous
        // code path skipped the cleanup block entirely when
        // `cleanup_connections` was false, leaving the mismatch
        // silent. It only becomes observable if a downstream client
        // happens to send a Parse on the same `DOORMAN_N` name; the
        // synthetic-response paths in this branch make that more
        // likely.
        if self.cleanup_state.needs_cleanup_prepare && !self.cleanup_connections {
            warn!(
                "[{}@{}] server pid={} has a prepared-statement cache mismatch but \
                 cleanup_server_connections=false - dropping backend to avoid \
                 SQLSTATE 42P05 on the next client Parse",
                self.address.username, self.address.pool_name, self.process_id
            );
            self.mark_bad("prepared cache mismatch with cleanup disabled");
            return Err(Error::ProtocolSyncError(format!(
                "Server {} (database: {}, user: {}) has a prepared-statement cache \
                 inconsistent with the client's view, and cleanup_server_connections \
                 is disabled so DEALLOCATE ALL cannot be sent. Dropping the backend.",
                self.address.host, self.address.database, self.address.username
            )));
        }

        let mut stmts: Vec<String> = Vec::new();

        if self.in_transaction() {
            warn!(
                "[{}@{}] server returned in transaction, rolling back pid={}",
                self.address.username, self.address.pool_name, self.process_id
            );
            stmts.push("ROLLBACK".to_string());
        }

        if self.cleanup_state.needs_cleanup() && self.cleanup_connections {
            // was `info!` - fires on every checkin that touched
            // a SET / PREPARE / DECLARE statement. On the profiling
            // (50-client SET-heavy simple-protocol workload) this single
            // line plus chrono timestamp formatting in `TextLogger::log`
            // bottlenecked the run at 1.4% CPU. Operator-relevant
            // information is preserved at `debug!` - flipping the level
            // when chasing a regression surfaces it as before.
            debug!(
                "[{}@{}] session state cleanup pid={}: {}",
                self.address.username, self.address.pool_name, self.process_id, self.cleanup_state
            );
            if self.cleanup_state.needs_cleanup_session_authorization {
                stmts.push("RESET SESSION AUTHORIZATION".to_string());
            }
            if self.cleanup_state.needs_cleanup_role
                || self.cleanup_state.needs_cleanup_session_authorization
                || self.cleanup_state.needs_cleanup_set
                || self.cleanup_state.needs_cleanup_prepare
                || self.cleanup_state.needs_cleanup_declare
            {
                stmts.push("RESET ROLE".to_string());
            }
            if self.cleanup_state.needs_cleanup_set {
                stmts.push("RESET ALL".to_string());
            }
            if self.cleanup_state.needs_cleanup_prepare {
                stmts.push("DEALLOCATE ALL".to_string());
            }
            if self.cleanup_state.needs_cleanup_declare {
                stmts.push("CLOSE ALL".to_string());
            }
        }

        Ok(stmts)
    }

    /// Execute the pool-shared pre-encoded frame when no session cleanup
    /// statements need to be combined with the release query.
    async fn send_release_query_only(
        &mut self,
        release_query: &ResolvedReleaseQuery,
    ) -> Result<(), Error> {
        if let Err(err) = self.small_simple_query_frame(release_query.frame()).await {
            const PREVIEW_CAP: usize = 200;
            let preview = combined_sql_preview(release_query.sql(), PREVIEW_CAP);
            let reason = format!("finalize_checkin release_query error: {err} on query: {preview}");
            warn!(
                "[{}@{}] finalize_checkin backend pid={} marked bad: {}",
                self.address.username, self.address.pool_name, self.process_id, reason
            );
            self.mark_bad(&reason);
            return Err(err);
        }

        if self.cleanup_state.needs_cleanup() {
            let reason = format!(
                "finalize_checkin release_query left backend cleanup state dirty: {}",
                self.cleanup_state
            );
            warn!(
                "[{}@{}] finalize_checkin backend pid={} marked bad: {}",
                self.address.username, self.address.pool_name, self.process_id, reason
            );
            self.mark_bad(&reason);
            return Err(Error::ProtocolSyncError(reason));
        }
        if self.in_transaction || self.in_copy_mode {
            let reason = format!(
                "finalize_checkin (release_query) left backend non-idle: \
                 in_transaction={}, in_copy={}",
                self.in_transaction, self.in_copy_mode
            );
            warn!(
                "[{}@{}] finalize_checkin backend pid={} marked bad: {}",
                self.address.username, self.address.pool_name, self.process_id, reason
            );
            self.mark_bad(&reason);
            return Err(Error::ProtocolSyncError(reason));
        }

        self.release_cleanup_pending = false;
        Ok(())
    }

    /// Send combined housekeeping statements in one round trip and update
    /// local cleanup and prepared-statement state after PostgreSQL confirms it.
    async fn send_checkin_cleanup(
        &mut self,
        stmts: Vec<String>,
        release_query_appended: Option<&str>,
    ) -> Result<(), Error> {
        let session_state_was_dirty =
            self.cleanup_state.needs_cleanup() && self.cleanup_connections;
        let needs_cleanup_prepare = self.cleanup_state.needs_cleanup_prepare;
        let mut internal_reset_all_sent = false;

        if !stmts.is_empty() {
            let internal_stmt_count = stmts.len() - usize::from(release_query_appended.is_some());
            internal_reset_all_sent = stmts[..internal_stmt_count]
                .iter()
                .any(|stmt| stmt == "RESET ALL");
            self.track_internal_checkin_cleanup_attribution(&stmts[..internal_stmt_count]);
            let combined = stmts.join(";\n") + ";";
            if let Err(err) = self.small_simple_query(&combined).await {
                // Operators debugging a misconfigured release_query (the
                // most common failure shape: `pg_variables` extension
                // missing, `pgv_free()` undefined) need to see WHICH
                // statement in the combined query produced the error and
                // what PostgreSQL said about it. Without this the log
                // line is just "finalize_checkin combined housekeeping
                // error: <transport err>" with no SQLSTATE / message
                // attribution. The combined-SQL preview is capped at
                // 200 bytes so a runaway release_query cannot blow up
                // the log line.
                const PREVIEW_CAP: usize = 200;
                let combined_preview = combined_sql_preview(&combined, PREVIEW_CAP);
                let sql_error = self
                    .last_sql_error
                    .as_ref()
                    .map(|(sqlstate, msg)| format!(" [SQLSTATE {sqlstate}: {msg}]"))
                    .unwrap_or_default();
                let path = if release_query_appended.is_some() {
                    "finalize_checkin (cleanup + release_query)"
                } else {
                    "checkin_cleanup"
                };
                let reason = format!("{path} SQL error{sql_error} on combined: {combined_preview}");
                warn!(
                    "[{}@{}] {} backend pid={} marked bad: {}",
                    self.address.username, self.address.pool_name, path, self.process_id, reason
                );
                self.mark_bad(&reason);
                return Err(err);
            }

            // Cache clear is gated on `session_state_was_dirty` (== DEALLOCATE
            // ALL was actually in the combined query), NOT on
            // `needs_cleanup_prepare` alone. Without this gate, when
            // `cleanup_connections=false` we'd wipe pg_doorman's view of the
            // server's prepared-statement cache while PostgreSQL keeps the
            // statements installed - the next client Parse on the same
            // deterministic `DOORMAN_N` name would hit SQLSTATE 42P05. The
            // `collect_*` helper already mark_bads + Errs the connection on
            // the (needs_cleanup_prepare && !cleanup_connections) shape, so
            // this code path is only reached when DEALLOCATE was actually
            // sent - the gate here is defence in depth in case a future
            // refactor adds another way into `send_*`.
            if needs_cleanup_prepare && session_state_was_dirty {
                self.registering_prepared_statement.clear();
                if let Some(cache) = self.prepared_statement_cache.as_mut() {
                    let cache_size = cache.len();
                    if cache_size > 0 {
                        info!(
                            "[{}@{}] clearing prepared statement cache pid={}: session state reset ({} entries)",
                            self.address.username,
                            self.address.pool_name,
                            self.process_id,
                            cache_size
                        );
                        cache.clear();
                    }
                }
            }
            if release_query_appended.is_some() && self.cleanup_state.needs_cleanup() {
                let reason = format!(
                    "finalize_checkin release_query left backend cleanup state dirty: {}",
                    self.cleanup_state
                );
                warn!(
                    "[{}@{}] finalize_checkin backend pid={} marked bad: {}",
                    self.address.username, self.address.pool_name, self.process_id, reason
                );
                self.mark_bad(&reason);
                return Err(Error::ProtocolSyncError(reason));
            }
            if session_state_was_dirty {
                self.cleanup_state.reset();
            }
        }

        if self.in_transaction || self.in_copy_mode {
            let path = if release_query_appended.is_some() {
                "finalize_checkin (cleanup + release_query)"
            } else {
                "checkin_cleanup"
            };
            let reason = format!(
                "{path} left backend non-idle after successful cleanup: \
                 in_transaction={}, in_copy={}",
                self.in_transaction, self.in_copy_mode
            );
            warn!(
                "[{}@{}] {} backend pid={} marked bad: {}",
                self.address.username, self.address.pool_name, path, self.process_id, reason
            );
            self.mark_bad(&reason);
            return Err(Error::ProtocolSyncError(reason));
        }
        if internal_reset_all_sent {
            self.server_parameters
                .remove_startup_only_params_after_session_reset();
        }
        if release_query_appended.is_some() {
            // Only a fully confirmed release round trip may disarm the
            // pending flag: transport errors, SQL errors, dirty cleanup
            // state and a non-idle transaction status all returned early
            // above. The plain checkin_cleanup path passes `None` here and
            // therefore never disarms the obligation.
            self.release_cleanup_pending = false;
        }
        Ok(())
    }

    /// Resolve a release query directly for isolated server tests.
    #[cfg(test)]
    pub(crate) fn set_release_query(&mut self, configured: Option<&str>) {
        self.release_query = resolve_release_query(configured);
    }

    pub(crate) fn set_resolved_release_query(
        &mut self,
        release_query: Option<ResolvedReleaseQuery>,
    ) {
        self.release_query = release_query;
    }

    /// Record that the checkout being handed out owes a `release_query`
    /// round trip before this backend may be reused. Called from
    /// `Pool::wrap_checkout`, which every checkout path funnels through.
    /// No-op when the release query is disabled; never disarms an
    /// already-armed flag.
    pub(crate) fn arm_release_cleanup(&mut self) {
        if self.release_query.is_some() {
            self.release_cleanup_pending = true;
        }
    }

    /// Setter for the DISCARD ALL interception switch. Invoked by
    /// `ServerPool::create` (and its fallback path) right after
    /// `Server::startup` so the per-backend cache mirrors
    /// `Pool.intercept_discard_all`.
    pub(crate) fn set_intercept_discard_all(&mut self, intercept: bool) {
        self.intercept_discard_all = intercept;
    }

    /// Read the cached interception switch. The DISCARD ALL fast path in
    /// `Client::handle_simple_query` calls this on every simple query.
    #[inline]
    pub(crate) fn intercept_discard_all(&self) -> bool {
        self.intercept_discard_all
    }

    #[inline]
    fn checkin_cleanup_metric_path(&self) -> &'static str {
        let has_release = self.release_query.is_some();
        let has_cleanup = self.in_transaction()
            || (self.cleanup_connections
                && (self.cleanup_state.needs_cleanup()
                    || self.has_pending_cache_entries
                    || !self.deferred_eviction_closes.is_empty()));
        match (has_release, has_cleanup) {
            (true, true) => "combined",
            (true, false) => "release_only",
            (false, true) => "cleanup_only",
            (false, false) => "empty",
        }
    }

    #[inline]
    fn checkin_cleanup_metric_result(result: &Result<(), Error>) -> &'static str {
        match result {
            Ok(()) => "ok",
            Err(Error::QueryError(_)) => "sql_error",
            Err(
                Error::SocketError(_)
                | Error::ConnectError(_)
                | Error::ConnectResourceExhausted(_)
                | Error::FlushTimeout
                | Error::ProxyTimeout,
            ) => "transport_error",
            Err(
                Error::ProtocolSyncError(_)
                | Error::ServerMessageParserError(_)
                | Error::ParseBytesError(_),
            ) => "protocol_error",
            Err(_) => "error",
        }
    }

    /// Run the regular `checkin_cleanup` and, if configured, the per-pool
    /// `release_query` before this backend goes back into the pool.
    ///
    /// A SQL failure from the release query marks the backend bad so a dirty
    /// session is never reused by another client. Async/expected-response state
    /// is forced off before the housekeeping statement, otherwise `small_simple_query`'s
    /// recv loop would not match the synchronous Sync/ReadyForQuery exchange.
    pub async fn finalize_checkin(&mut self) -> Result<(), Error> {
        let path = self.checkin_cleanup_metric_path();
        let started = quanta::Instant::now();
        let result = self.finalize_checkin_inner().await;
        crate::web::metrics::observe_checkin_cleanup(
            self.address.username.as_str(),
            self.address.database.as_str(),
            path,
            Self::checkin_cleanup_metric_result(&result),
            started.elapsed().as_secs_f64(),
        );
        result
    }

    async fn finalize_checkin_inner(&mut self) -> Result<(), Error> {
        // A release-only check-in sends the pool-shared frame directly.
        // Dirty sessions append release_query to the cleanup statements and
        // execute the combined SQL in one round trip.
        let mut stmts = self.collect_checkin_cleanup_sqls()?;
        let release_query = self.release_query.clone();
        if stmts.is_empty() {
            self.set_async_mode(false);
            self.set_expected_responses(0);
            self.in_transaction = false;
            self.in_copy_mode = false;
            return match release_query.as_ref() {
                Some(release_query) => self.send_release_query_only(release_query).await,
                None => {
                    debug_assert!(!self.release_cleanup_pending);
                    Ok(())
                }
            };
        }

        if let Some(ref release_query) = release_query {
            stmts.push(release_query.sql().to_string());
        }

        // Housekeeping queries must travel through the synchronous Sync /
        // ReadyForQuery exchange `small_simple_query` understands.
        self.set_async_mode(false);
        self.set_expected_responses(0);

        if let Err(err) = self
            .send_checkin_cleanup(stmts, release_query.as_ref().map(ResolvedReleaseQuery::sql))
            .await
        {
            warn!(
                "[{}@{}] finalize_checkin failed pid={}: {err}",
                self.address.username, self.address.pool_name, self.process_id
            );
            return Err(err);
        }

        Ok(())
    }

    /// We don't buffer all of server responses, e.g. COPY OUT produces too much data.
    /// The client is responsible to call `self.recv()` while this method returns true.
    #[inline(always)]
    pub fn is_data_available(&self) -> bool {
        self.data_available
    }

    /// Switch to async mode, flushing messages as soon
    /// as we receive them without buffering or waiting for "ReadyForQuery".
    #[inline(always)]
    pub fn set_async_mode(&mut self, async_mode: bool) {
        self.async_mode = async_mode
    }

    /// Sets the number of expected responses in async mode.
    /// Calculated from the batch operations before sending to the server.
    #[inline(always)]
    pub fn set_expected_responses(&mut self, count: u32) {
        self.expected_responses = count;
        self.expected_response_sequence.clear();
        self.expected_response_sequence.extend(std::iter::repeat_n(
            AsyncExpectedResponse::Operation,
            count as usize,
        ));
    }

    /// Sets the ordered terminal response kinds expected in async mode.
    pub(crate) fn set_expected_response_sequence<I>(&mut self, sequence: I)
    where
        I: IntoIterator<Item = AsyncExpectedResponse>,
    {
        self.expected_response_sequence.clear();
        self.expected_response_sequence.extend(sequence);
        self.expected_responses = self.expected_response_sequence.len() as u32;
    }

    /// Returns a snapshot of the async expected response sequence.
    pub(crate) fn expected_response_sequence(&self) -> Vec<AsyncExpectedResponse> {
        self.expected_response_sequence.iter().copied().collect()
    }

    /// Returns the current number of expected responses.
    #[inline(always)]
    pub fn expected_responses(&self) -> u32 {
        self.expected_responses
    }

    /// Decrements the expected response counter.
    /// Called when receiving terminating messages in async mode.
    #[inline(always)]
    pub fn decrement_expected(&mut self) {
        self.decrement_expected_kind(AsyncExpectedResponse::Operation);
    }

    /// Decrements expected response only when RowDescription/NoData belongs to Describe.
    #[inline(always)]
    pub fn decrement_expected_describe_terminal(&mut self) {
        self.decrement_expected_kind(AsyncExpectedResponse::Describe);
    }

    #[inline(always)]
    fn decrement_expected_kind(&mut self, expected: AsyncExpectedResponse) {
        if self.expected_response_sequence.front() == Some(&expected) {
            self.expected_response_sequence.pop_front();
            self.expected_responses = self.expected_responses.saturating_sub(1);
        } else if self.expected_response_sequence.is_empty() {
            self.expected_responses = self.expected_responses.saturating_sub(1);
        }
    }

    /// Resets expected responses to 0.
    /// Called on ErrorResponse in async mode since error aborts remaining operations.
    #[inline(always)]
    pub fn reset_expected_responses(&mut self) {
        self.expected_responses = 0;
        self.expected_response_sequence.clear();
    }

    pub(crate) fn track_reset_cleanup_commands<I>(&mut self, commands: I)
    where
        I: IntoIterator<Item = ResetCleanupCommand>,
    {
        self.pending_reset_cleanup_commands.extend(commands);
    }

    pub(crate) fn track_set_cleanup_commands<I>(&mut self, commands: I)
    where
        I: IntoIterator<Item = SetCleanupCommand>,
    {
        self.pending_set_cleanup_commands.extend(commands);
    }

    #[inline(always)]
    pub(crate) fn pop_reset_cleanup_command(&mut self) -> Option<ResetCleanupCommand> {
        self.pending_reset_cleanup_commands.pop_front()
    }

    #[inline(always)]
    pub(crate) fn pop_set_cleanup_command(&mut self) -> Option<SetCleanupCommand> {
        self.pending_set_cleanup_commands.pop_front()
    }

    #[inline(always)]
    pub(crate) fn clear_reset_cleanup_commands(&mut self) {
        self.pending_reset_cleanup_commands.clear();
    }

    #[inline(always)]
    pub(crate) fn clear_set_cleanup_commands(&mut self) {
        self.pending_set_cleanup_commands.clear();
    }

    #[inline(always)]
    pub(crate) fn clear_internal_set_cleanup_state(&mut self) {
        self.pending_set_cleanup_commands.clear();
        self.cleanup_state.needs_cleanup_set = false;
    }

    fn add_prepared_statement_to_cache(&mut self, name: &str) -> Option<String> {
        prepared_statements::add_to_cache(&mut self.prepared_statement_cache, &self.stats, name)
    }

    pub(crate) fn remove_prepared_statement_from_cache(&mut self, name: &str) {
        prepared_statements::remove_from_cache(
            &mut self.prepared_statement_cache,
            &self.stats,
            name,
        );
    }

    pub(crate) fn take_rejected_prepared_statement_names(&mut self) -> Vec<String> {
        std::mem::take(&mut self.rejected_prepared_statement_names)
    }

    /// Register a prepared statement on the server.
    ///
    /// # Arguments
    /// * `parse` - The Parse message containing query text and parameters
    /// * `server_name` - The name to use on the server (may differ from parse.name for async clients)
    /// * `should_send_parse_to_server` - Whether to actually send Parse to server
    pub async fn register_prepared_statement(
        &mut self,
        parse: &Parse,
        server_name: &str,
        should_send_parse_to_server: bool,
    ) -> Result<(), Error> {
        self.register_prepared_statement_with_timeout(
            parse,
            server_name,
            should_send_parse_to_server,
            HOUSEKEEPING_TIMEOUT,
        )
        .await
    }

    async fn register_prepared_statement_with_timeout(
        &mut self,
        parse: &Parse,
        server_name: &str,
        should_send_parse_to_server: bool,
        housekeeping_timeout: Duration,
    ) -> Result<(), Error> {
        if !self.has_prepared_statement(server_name) {
            if should_send_parse_to_server && self.is_async() {
                let reason = format!(
                    "cannot register prepared statement `{server_name}` with backend-only Sync \
                     while frontend Flush async cycle is open"
                );
                self.mark_bad(&reason);
                return Err(Error::ProtocolSyncError(reason));
            }

            self.registering_prepared_statement
                .push_back(server_name.to_string());

            // take the already-serialized Parse buffer as the
            // owned wire buffer directly instead of allocating a fresh
            // empty BytesMut and `extend_from_slice`-copying the whole
            // Parse frame into it. `to_bytes_with_name` returns an owned
            // BytesMut, so this saves one full-Parse-sized memcpy per
            // cold Parse. When we are not sending Parse to the server the
            // buffer stays empty (zero-cap, no allocation).
            let mut bytes = if should_send_parse_to_server {
                // Use server_name instead of parse.name for async clients
                parse.to_bytes_with_name(server_name)?
            } else {
                BytesMut::new()
            };

            // Track that we added to cache without sending Parse to PostgreSQL.
            // The actual Parse is deferred in the client buffer until Sync/Flush.
            // If the client disconnects before flushing, checkin_cleanup will
            // detect this flag and trigger DEALLOCATE ALL to fix the desync.
            if !should_send_parse_to_server {
                self.has_pending_cache_entries = true;
            }

            // If we evict something, defer the Close until after the current batch
            // completes (Sync/Flush). The evicted statement still exists on PostgreSQL,
            // so any Bind referencing it in the client buffer will succeed.
            // send_deferred_eviction_closes() sends the Close after Sync.
            if let Some(evicted_name) = self.add_prepared_statement_to_cache(server_name) {
                self.queue_deferred_eviction_close(evicted_name);
            };

            // If we have a parse or close we need to send to the server, send them and sync
            if !bytes.is_empty() {
                bytes.extend_from_slice(&sync());

                // Temporarily disable async mode so that recv() waits for
                // ReadyForQuery instead of exiting immediately when
                // expected_responses == 0.  Without this, CloseComplete and
                // ReadyForQuery from eviction stay in the TCP buffer and
                // corrupt the next server roundtrip.
                let was_async = self.is_async();
                let saved_expected = self.expected_response_sequence();
                if was_async {
                    self.set_async_mode(false);
                }

                // Bound this internal Parse+Sync round-trip with
                // the same housekeeping deadline used by small_simple_query /
                // send_deferred_eviction_closes. A backend that accepts the
                // Parse bytes and then stalls before ParseComplete/RFQ must be
                // evicted instead of pinning the checked-out client task.
                let deadline = tokio::time::Instant::now() + housekeeping_timeout;
                self.begin_internal_round_trip();

                match tokio::time::timeout_at(deadline, self.send_and_flush(&bytes)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => return Err(err),
                    Err(_) => {
                        self.mark_bad("housekeeping send timeout in register_prepared_statement");
                        return Err(Error::SocketError(
                            "timeout sending prepared-statement Parse".to_string(),
                        ));
                    }
                }

                let mut noop = tokio::io::sink();
                loop {
                    match tokio::time::timeout_at(deadline, self.recv(&mut noop, None)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(err)) => {
                            self.mark_bad(&format!(
                                "transport error in register_prepared_statement recv: {err}"
                            ));
                            return Err(err);
                        }
                        Err(_) => {
                            self.mark_bad(
                                "housekeeping recv timeout in register_prepared_statement",
                            );
                            return Err(Error::SocketError(
                                "timeout draining prepared-statement Parse response".to_string(),
                            ));
                        }
                    }

                    if !self.is_data_available() {
                        break;
                    }
                }

                // Restore async mode state for the ongoing client pipeline.
                if was_async {
                    self.set_async_mode(true);
                    self.set_expected_response_sequence(saved_expected);
                }
                self.finish_internal_round_trip();
            }
        };

        // If it's not there, something went bad, I'm guessing bad syntax or permissions error
        // on the server.
        if !self.has_prepared_statement(server_name) {
            Err(Error::PreparedStatementError)
        } else {
            Ok(())
        }
    }

    /// Claim this server as mine for the purposes of query cancellation.
    pub fn claim(&mut self, process_id: i32, secret_key: i32) {
        self.client_server_map.insert(
            (process_id, secret_key),
            CancelTarget {
                process_id: self.process_id,
                secret_key: self.secret_key,
                host: self.address.host.clone(),
                port: self.address.port,
                server_tls: self.address.server_tls.clone(),
                connected_with_tls: self.connected_with_tls,
                pool_name: self.address.pool_name.clone(),
            },
        );
    }

    /// queue a server prepared-statement name for backend
    /// Close on the next Sync. Used by the per-client Named cap
    /// eviction path so the PG-side prepared cache doesn't leak the
    /// orphan name. Idempotent - duplicate names accumulate harmlessly
    /// (the drain loop dedupes via wire-level Close which is safe to
    /// repeat on an absent name). Removes the name from the local LRU
    /// immediately; `has_prepared_statement` still reports it present
    /// through `deferred_eviction_closes` until the Close reaches PostgreSQL.
    pub fn queue_deferred_eviction_close(&mut self, server_name: String) {
        self.remove_prepared_statement_from_cache(&server_name);
        self.deferred_eviction_closes.insert(server_name);
    }

    /// Determines if the server already has a prepared statement with the given name.
    /// Checks both the LRU cache and the deferred eviction list (statements evicted
    /// from LRU but not yet Closed on PostgreSQL — they still exist there).
    #[inline]
    pub fn has_prepared_statement(&mut self, name: &str) -> bool {
        // O(1) HashSet lookup (was O(N) Vec linear scan).
        if self.deferred_eviction_closes.contains(name) {
            self.stats.prepared_cache_hit();
            return true;
        }
        prepared_statements::has(&mut self.prepared_statement_cache, &self.stats, name)
    }

    /// Send Close+Sync for all deferred eviction entries and consume responses.
    /// Called after the client batch is flushed (Sync/Flush) so that Binds
    /// referencing evicted statements have already been processed by PostgreSQL.
    pub async fn send_deferred_eviction_closes(&mut self) -> Result<(), Error> {
        self.send_deferred_eviction_closes_with_timeout(HOUSEKEEPING_TIMEOUT)
            .await
    }

    /// Inner body of [`send_deferred_eviction_closes`] with the round-trip
    /// deadline injected so unit tests can drive it with a short timeout.
    async fn send_deferred_eviction_closes_with_timeout(
        &mut self,
        housekeeping_timeout: Duration,
    ) -> Result<(), Error> {
        if self.deferred_eviction_closes.is_empty() {
            return Ok(());
        }
        if self.in_copy_mode() {
            debug!(
                "[{}@{}] deferring {} prepared-statement Close messages while backend is in COPY mode pid={}",
                self.address.username,
                self.address.pool_name,
                self.deferred_eviction_closes.len(),
                self.process_id,
            );
            return Ok(());
        }
        if self.is_async() {
            debug!(
                "[{}@{}] deferring {} prepared-statement Close messages while backend is in Flush async mode pid={}",
                self.address.username,
                self.address.pool_name,
                self.deferred_eviction_closes.len(),
                self.process_id,
            );
            return Ok(());
        }

        let mut bytes = BytesMut::new();
        for name in self.deferred_eviction_closes.drain() {
            let close_bytes: BytesMut = Close::new(&name).try_into()?;
            bytes.extend_from_slice(&close_bytes);
        }
        bytes.extend_from_slice(&sync());

        let was_async = self.is_async();
        let saved_expected = self.expected_response_sequence();
        if was_async {
            self.set_async_mode(false);
        }

        // bound this internal Close+Sync
        // round-trip with a single deadline, exactly like small_simple_query /
        // check_alive / swallow_set_response. Without it a backend that stalls
        // after receiving Close+Sync blocks the client task forever with the
        // backend still checked out. On expiry mark the backend bad so it is
        // evicted instead of pinning the task.
        let deadline = tokio::time::Instant::now() + housekeeping_timeout;
        self.begin_internal_round_trip();

        match tokio::time::timeout_at(deadline, self.send_and_flush(&bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                self.mark_bad("housekeeping send timeout in send_deferred_eviction_closes");
                return Err(Error::SocketError(
                    "timeout sending deferred eviction Close batch".to_string(),
                ));
            }
        }

        let mut noop = tokio::io::sink();
        loop {
            match tokio::time::timeout_at(deadline, self.recv(&mut noop, None)).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    self.mark_bad(&format!(
                        "transport error in send_deferred_eviction_closes recv: {err}"
                    ));
                    return Err(err);
                }
                Err(_) => {
                    self.mark_bad("housekeeping recv timeout in send_deferred_eviction_closes");
                    return Err(Error::SocketError(
                        "timeout draining deferred eviction Close response".to_string(),
                    ));
                }
            }
            if !self.is_data_available() {
                break;
            }
        }

        if was_async {
            self.set_async_mode(true);
            self.set_expected_response_sequence(saved_expected);
        }

        self.finish_internal_round_trip();

        Ok(())
    }

    pub async fn sync_parameters(&mut self, parameters: &ServerParameters) -> Result<(), Error> {
        let mut parameter_diff = self.server_parameters.compare_params(parameters);

        // Configured startup_parameters win over client StartupMessage values.
        if !self.operator_managed_startup_keys.is_empty() {
            parameter_diff.retain(|k, _| !self.operator_managed_startup_keys.contains(k));
        }

        if parameter_diff.is_empty() {
            crate::web::metrics::inc_sync_params_skipped();
            return Ok(());
        }

        // deterministic SET/RESET ordering.
        // `compare_params` returns a HashMap whose iteration order
        // is randomised per process (Rust's default SipHash). Two
        // backends with identical client-side parameter diff
        // emitted the same housekeeping SQL in different
        // orders -> `pg_stat_statements` recorded distinct queryid
        // entries for what is logically the same query, fragmenting
        // the planner cache and polluting p99 dashboards. Sort by
        // key before iteration for stable text.
        let mut diff_sorted: Vec<_> = parameter_diff.iter().collect();
        diff_sorted.sort_by(|a, b| a.0.cmp(b.0));

        use std::fmt::Write as _;
        let mut query = String::new();
        for (key, action) in &diff_sorted {
            match action {
                crate::server::parameters::ParamAction::SetTo(value) => {
                    // GUC keys come from a canonicalised whitelist, so they
                    // never need quoting. Values may contain apostrophes,
                    // backslashes, or anything else a client put in
                    // application_name / search_path - dollar-quoting is the
                    // only literal form PostgreSQL leaves untouched.
                    // propagate fail-closed Err from
                    // exhaust-cap path.
                    let literal = sql_string_literal(value)?;
                    let _ = write!(query, "SET {key} TO {literal};");
                }
                crate::server::parameters::ParamAction::Reset => {
                    let _ = write!(query, "RESET {key};");
                }
            }
        }

        let sync_started = Instant::now();
        let res = self.small_simple_query(&query).await;
        let sync_elapsed = sync_started.elapsed();

        match &res {
            Ok(()) => {
                crate::web::metrics::inc_sync_params_applied();
                crate::web::metrics::observe_sync_params_rtt_seconds(sync_elapsed.as_secs_f64());
                // Mirror successful SET/RESET actions because PostgreSQL does
                // not emit ParameterStatus for most planner GUCs, including
                // search_path.
                for (key, action) in parameter_diff {
                    match action {
                        crate::server::parameters::ParamAction::SetTo(value) => {
                            let _ = self.server_parameters.set_param(&key, value, true);
                        }
                        crate::server::parameters::ParamAction::Reset => {
                            self.server_parameters.remove_param(&key);
                        }
                    }
                }
            }
            Err(err) => {
                // A failed sync_parameters leaves the backend in a half-applied
                // GUC state (some SETs may have landed before the rejected one),
                // and the snapshot in `self.server_parameters` is stale. Reusing
                // this backend for another client would silently expose the
                // wrong session settings. Mark it bad so the pool drops it.
                warn!(
                    "[{}@{}] sync_parameters failed pid={}: {err}",
                    self.address.username, self.address.pool_name, self.process_id
                );
                self.mark_bad("sync_parameters error");
                // do NOT reset cleanup_state on the
                // failure path. If a future refactor demotes
                // `mark_bad` to a softer signal, the cleared flags
                // would silently let the dirty backend re-enter the
                // pool. Leave armed flags intact so checkin_cleanup
                // still runs the appropriate RESET/DEALLOCATE/CLOSE
                // sequence.
                return res;
            }
        }

        self.cleanup_state.reset();

        res
    }

    /// Issue a query cancellation request to the server.
    /// Uses a separate connection that's not part of the connection pool.
    pub async fn cancel(
        host: &str,
        port: u16,
        process_id: i32,
        secret_key: i32,
        server_tls: &tls::ServerTlsConfig,
        connected_with_tls: bool,
        pool_name: &str,
    ) -> Result<(), Error> {
        startup_cancel::cancel(
            host,
            port,
            process_id,
            secret_key,
            server_tls,
            connected_with_tls,
            pool_name,
        )
        .await
    }

    /// Reissue a cancel against this backend only while the original client
    /// cancel marker is still present. The marker keeps the backend quarantined
    /// until check-in, so a second cancel cannot be routed to another client.
    pub(crate) async fn reissue_cancel_if_marked(&self) -> Option<Result<(), Error>> {
        if !CANCELED_PIDS.contains_key(&self.process_id) {
            return None;
        }

        Some(
            Self::cancel(
                &self.address.host,
                self.address.port,
                self.process_id,
                self.secret_key,
                &self.address.server_tls,
                self.connected_with_tls,
                &self.address.pool_name,
            )
            .await,
        )
    }

    // Marks a connection as needing cleanup at checkin
    pub fn mark_dirty(&mut self) {
        self.cleanup_state.set_true();
    }

    /// Pretend to be the Postgres client and connect to the server given host, port and credentials.
    /// Perform the authentication and return the server in a ready for query state.
    ///
    /// `startup_parameters` is the resolved cascade
    /// (`general` -> pool -> auth_query). It is sent in the backend
    /// `StartupMessage`. If PostgreSQL rejects a value, pg_doorman forwards
    /// the `ErrorResponse` unchanged.
    #[allow(clippy::too_many_arguments)]
    pub async fn startup(
        address: &Address,
        user: &User,
        database: &str,
        client_server_map: ClientServerMap,
        stats: Arc<ServerStats>,
        cleanup_connections: bool,
        log_client_parameter_status_changes: bool,
        server_prepared_statement_cache_size: usize,
        application_name: String,
        session_mode: bool,
        startup_parameters: &std::collections::BTreeMap<String, String>,
        operator_managed_startup_keys: Arc<HashSet<String>>,
    ) -> Result<Server, Error> {
        let config = get_config();
        let max_memory_usage = config.general.max_memory_usage.as_bytes();

        log::debug!(
            "[{}@{}] server startup connecting to {}:{} server_tls_mode={}",
            user.username,
            database,
            address.host,
            address.port,
            address.server_tls.mode
        );

        let mut stream = if address.host.starts_with('/') {
            create_unix_stream_inner(&address.host, address.port).await?
        } else {
            create_tcp_stream_inner(
                &address.host,
                address.port,
                &address.server_tls,
                &address.pool_name,
            )
            .await?
        };

        let connected_with_tls = matches!(&stream, StreamInner::TCPTls { .. });
        log::debug!(
            "[{}@{}] server connection to {}:{} established tls={}",
            user.username,
            database,
            address.host,
            address.port,
            connected_with_tls
        );

        let username = user
            .server_username
            .as_ref()
            .unwrap_or(&user.username)
            .clone();
        // StartupMessage. The auth phase is wall-clock from the
        // outbound write here to the inbound AuthenticationOK ('R' with
        // code 0); see the `'R'` branch below.
        let auth_started = Instant::now();
        let mut startup_started: Option<Instant> = None;

        startup(
            &mut stream,
            username.as_str(),
            database,
            application_name.as_str(),
            startup_parameters,
        )
        .await?;

        let mut process_id: i32 = 0;
        let mut secret_key: i32 = 0;
        let server_identifier =
            ServerIdentifier::new(username.clone(), database, &address.pool_name);

        let backend_auth_snapshot = address.backend_auth.as_ref().map(|ba| ba.read().clone());

        let mut scram_client_auth = match &backend_auth_snapshot {
            Some(BackendAuthMethod::ScramPassthrough(client_key)) => {
                Some(ScramSha256::from_client_key(client_key.clone()))
            }
            Some(BackendAuthMethod::ScramPending) => {
                // SCRAM passthrough configured but ClientKey not yet available.
                // Fall through to server_password if available; otherwise None
                // (backend SASL auth will fail with a clear error).
                warn!(
                    "[{}@{}] backend connection attempted before first client SCRAM auth (ScramPending), \
                     falling back to server_password",
                    address.username, address.pool_name
                );
                if let (Some(_), Some(server_password)) =
                    (&user.server_username, &user.server_password)
                {
                    Some(ScramSha256::new(server_password))
                } else {
                    None
                }
            }
            _ => {
                // Existing logic: create from server_password
                if let (Some(_), Some(server_password)) =
                    (&user.server_username, &user.server_password)
                {
                    Some(ScramSha256::new(server_password))
                } else {
                    None
                }
            }
        };
        let mut server_parameters = ServerParameters::new();

        loop {
            let code = match stream.read_u8().await {
                Ok(code) => code as char,
                Err(err) => {
                    return Err(Error::ServerStartupError(
                        format!("Failed to read message code during server startup: {err}"),
                        server_identifier.clone(),
                    ));
                }
            };

            let len = match stream.read_i32().await {
                Ok(len) => len,
                Err(err) => {
                    return Err(Error::ServerStartupError(
                        format!("Failed to read message length during server startup: {err}"),
                        server_identifier.clone(),
                    ));
                }
            };

            match code {
                // Authentication
                'R' => {
                    let auth_code = stream.read_i32().await.map_err(|_| {
                        Error::ServerStartupError(
                            "Failed to read authentication code from server".into(),
                            server_identifier.clone(),
                        )
                    })?;

                    handle_authentication(
                        &mut stream,
                        auth_code,
                        len,
                        user,
                        &mut scram_client_auth,
                        &server_identifier,
                        backend_auth_snapshot.as_ref(),
                    )
                    .await?;

                    // auth_code 0 is AuthenticationOK; the auth phase
                    // ends here and the post-auth startup phase begins.
                    // Setting startup_started only on the first OK
                    // shields against any future code path that might
                    // read another 'R' afterwards.
                    if auth_code == 0 && startup_started.is_none() {
                        crate::web::metrics::observe_backend_create_phase(
                            "auth",
                            auth_started.elapsed().as_secs_f64(),
                        );
                        startup_started = Some(Instant::now());
                    }
                }

                // ErrorResponse during startup. Keep SQLSTATE class 57P
                // on the fallback path; other startup errors are forwarded
                // to the client as PostgreSQL returned them.
                'E' => {
                    let mut bytes = read_message_data_with_memory_limit(
                        &mut stream,
                        code as u8,
                        len,
                        max_memory_usage,
                    )
                    .await?;
                    let _ = bytes.get_u8();
                    let _ = bytes.get_i32();
                    let Ok(msg) = PgErrorMsg::parse(&bytes) else {
                        return Err(Error::ServerStartupError(
                            "startup ErrorResponse".to_string(),
                            server_identifier.clone(),
                        ));
                    };

                    if msg.code.starts_with("57P") {
                        return Err(Error::ServerUnavailableError(
                            msg.message,
                            server_identifier.clone(),
                        ));
                    }

                    // Identify the failing parameter for logs and metrics.
                    //
                    // First parse the common English `parameter "<name>"`
                    // phrase, then fall back to looking for any sent key in
                    // double quotes. The fallback covers translated
                    // `lc_messages` where PostgreSQL still quotes the name.
                    let matched_key = if startup_parameters.is_empty() {
                        None
                    } else {
                        crate::server::startup_error::extract_parameter_name(&msg.message)
                            .filter(|n| startup_parameters.contains_key(n))
                            .or_else(|| {
                                crate::server::startup_error::match_sent_key_in_message(
                                    &msg.message,
                                    startup_parameters.keys(),
                                )
                            })
                    };
                    if let Some(param_name) = matched_key {
                        warn!(
                            "[{}@{}] PG rejected operator-supplied startup \
                             parameter=\"{}\" sqlstate={} message=\"{}\"; the \
                             error is being forwarded to the client. Fix the \
                             parameter in general/pool/auth_query.",
                            address.username, address.pool_name, param_name, msg.code, msg.message,
                        );
                        // Bound Prometheus cardinality: the full SQLSTATE
                        // is still forwarded to the client and logged, but
                        // metrics keep only startup-parameter classes.
                        let sqlstate_label =
                            crate::server::startup_error::startup_parameter_sqlstate_metric_label(
                                &msg.code,
                            );
                        crate::web::metrics::observe_backend_startup_parameter_error(
                            address.pool_name.as_str(),
                            sqlstate_label,
                        );
                        return Err(Error::ServerStartupParameterRejection {
                            sqlstate: msg.code,
                            message: msg.message,
                            server_identifier: server_identifier.clone(),
                        });
                    }

                    return Err(Error::ServerStartupError(
                        format!("{}: {}", msg.code, msg.message),
                        server_identifier.clone(),
                    ));
                }

                // Notice
                'N' => {
                    let mut msg = read_message_data_with_memory_limit(
                        &mut stream,
                        code as u8,
                        len,
                        max_memory_usage,
                    )
                    .await?;
                    let _ = msg.get_u8();
                    let _ = msg.get_i32();
                    if let Ok(msg) = PgErrorMsg::parse(&msg) {
                        warn!(
                            "[{}@{}] startup notice: severity={}, code={}, message={}",
                            address.username,
                            address.pool_name,
                            msg.severity,
                            msg.code,
                            msg.message
                        )
                    };
                }

                // ParameterStatus
                'S' => {
                    let mut bytes = read_message_data_with_memory_limit(
                        &mut stream,
                        code as u8,
                        len,
                        max_memory_usage,
                    )
                    .await?;
                    let _ = bytes.get_u8();
                    let _ = bytes.get_i32();
                    // Surface a truncated startup ParameterStatus frame as a
                    // ServerStartupError so the caller drops this backend cleanly.
                    let key = bytes.read_string().map_err(|err| {
                        Error::ServerStartupError(
                            format!("malformed ParameterStatus key during startup: {err}"),
                            server_identifier.clone(),
                        )
                    })?;
                    let value = bytes.read_string().map_err(|err| {
                        Error::ServerStartupError(
                            format!("malformed ParameterStatus value during startup: {err}"),
                            server_identifier.clone(),
                        )
                    })?;

                    // Save the parameter so we can pass it to the client later.
                    server_parameters.set_param(key, value, true);
                }

                // BackendKeyData
                'K' => {
                    // a canonical BackendKeyData frame
                    // is exactly 12 bytes (header 8 + two i32s). A
                    // hostile / MITM / buggy backend can send 'K' with
                    // a different declared length; without validation
                    // the handler consumes exactly 8 body bytes,
                    // leaving any extra bytes in the stream where the
                    // next iteration interprets them as the start of
                    // the following frame - silent protocol desync.
                    if len != 12 {
                        return Err(Error::ServerStartupError(
                            format!("BackendKeyData len must be 12, got {len}"),
                            server_identifier.clone(),
                        ));
                    }
                    // The frontend must save these values if it wishes to be able to issue CancelRequest messages later.
                    process_id = stream.read_i32().await.map_err(|_| {
                        Error::ServerStartupError(
                            "failed to read process ID during startup".into(),
                            server_identifier.clone(),
                        )
                    })?;

                    secret_key = stream.read_i32().await.map_err(|_| {
                        Error::ServerStartupError(
                            "failed to read secret key during startup".into(),
                            server_identifier.clone(),
                        )
                    })?;
                }

                // ReadyForQuery
                'Z' => {
                    let _idle = read_message_data_with_memory_limit(
                        &mut stream,
                        code as u8,
                        len,
                        max_memory_usage,
                    )
                    .await?;

                    // Close out the post-auth startup phase. We expect
                    // startup_started to be set by the AuthenticationOK
                    // branch above; if it isn't (a backend that skipped
                    // sending AuthenticationOK), fall back to the auth
                    // start so the metric still reflects total time
                    // beyond TCP/TLS instead of going silent.
                    let phase_started = startup_started.unwrap_or(auth_started);
                    crate::web::metrics::observe_backend_create_phase(
                        "startup",
                        phase_started.elapsed().as_secs_f64(),
                    );

                    let server = Server {
                        address: address.to_owned(),
                        // 64 KiB buffered stream cuts refill
                        // syscalls 8× on bulk-read paths (1M-row SELECT
                        // saw 5.85% CPU on `BufStream::poll_read`).
                        stream: std::mem::ManuallyDrop::new(BufStream::with_capacity(
                            BUF_STREAM_CAPACITY,
                            BUF_STREAM_CAPACITY,
                            stream,
                        )),
                        buffer: BytesMut::with_capacity(BUFFER_FLUSH_THRESHOLD),
                        read_buf: BytesMut::with_capacity(BUFFER_FLUSH_THRESHOLD),
                        server_parameters,
                        process_id,
                        secret_key,
                        in_transaction: false,
                        command_complete_in_transaction: false,
                        in_copy_mode: false,
                        data_available: false,
                        bad: false,
                        async_mode: false,
                        expected_responses: 0,
                        expected_response_sequence: VecDeque::new(),
                        cleanup_state: CleanupState::new(),
                        pending_set_cleanup_commands: VecDeque::new(),
                        pending_reset_cleanup_commands: VecDeque::new(),
                        pending_cleanup_disarms: PendingCleanupDisarms::default(),
                        response_cycle_had_error: false,
                        client_server_map,
                        connected_at: chrono::offset::Utc::now().naive_utc(),
                        stats,
                        application_name,
                        last_activity: SystemTime::now(),
                        last_activity_quanta: quanta::Instant::now(),
                        cleanup_connections,
                        log_client_parameter_status_changes,
                        prepared_statement_cache: match server_prepared_statement_cache_size {
                            0 => None,
                            // ahash-backed LRU on the hot
                            // per-server prepared-name cache.
                            _ => Some(LruCache::with_hasher(
                                NonZeroUsize::new(server_prepared_statement_cache_size).unwrap(),
                                ahash::RandomState::new(),
                            )),
                        },
                        registering_prepared_statement: VecDeque::new(),
                        rejected_prepared_statement_names: Vec::new(),
                        has_pending_cache_entries: false,
                        deferred_eviction_closes: std::collections::HashSet::new(),
                        connected_with_tls,
                        session_mode,
                        max_message_size: config.general.message_size_to_be_stream.as_bytes()
                            as i32,
                        pending_large_message: None,
                        close_reason: None,
                        override_lifetime_ms: None,
                        operator_managed_startup_keys,
                        last_sql_error: None,
                        release_query: None,
                        release_cleanup_pending: false,
                        intercept_discard_all: true,
                        internal_round_trip_in_flight: false,
                    };
                    server.stats.update_process_id(process_id);
                    server.stats.set_tls(connected_with_tls);

                    return Ok(server);
                }

                // We have an unexpected message from the server during this exchange.
                _ => {
                    error!("[{}@{}] unexpected message code '{}' (ASCII: {}) during server startup to {}:{}", server_identifier.username, server_identifier.pool_name, code, code as u8, address.host, address.port);
                    return Err(Error::ProtocolSyncError(format!(
                        "Received unexpected message code '{}' (ASCII: {}) during server startup. This may indicate an incompatible PostgreSQL server version or protocol.",
                        code, code as u8
                    )));
                }
            };
        }
    }
}

impl Drop for Server {
    /// Try to do a clean shut down. Best effort because
    /// the socket is in non-blocking mode, so it may not be ready
    /// for a write.
    fn drop(&mut self) {
        // Update statistics
        self.stats.disconnect();
        // Evict the debug PROTOCOL_STATES entry for this backend
        // unconditionally. The map is keyed by `process_id` and would
        // otherwise grow unbounded across reloads / long uptimes: once an
        // entry is inserted during a DEBUG-enabled window it survives the
        // log level being lowered, since the regular `cleanup_protocol_state`
        // path intentionally keeps entries alive in case the same backend
        // is handed to another client.
        crate::utils::debug_messages::remove_protocol_state(self.process_id);
        // gate behind `self.bad`. 's invariant is
        // that a cancelled pid stays in the set until the NEXT checkout
        // notices it (`transaction.rs:1350` `mark_bad` + retry). When the
        // backend is being dropped because it is already bad (recycle
        // failed, eviction killed it, mark_bad ran) the pid is gone for
        // good and the entry is genuinely stale - remove it so the
        // shard does not grow unbounded across long uptimes. When the
        // backend is being dropped on a clean path (pool shutdown / FD
        // pressure / explicit eviction of a healthy connection) we keep
        // the entry: a cancel may still be in flight (cancel pipeline
        // is now bounded by F11 at 5s, but the window is real), and the
        // pid being recycled by PG for an unrelated session must not
        // see this flag - that's the next checkout's check job to
        // discriminate, and the F14 cap/TTL bounds the worst-case
        // accumulation.
        if self.bad {
            CANCELED_PIDS.remove(&self.process_id);
        }
        // graceful Terminate. Historical behaviour used `try_write`
        // and silently swallowed `WouldBlock` (treating it as if the
        // Terminate had been sent), so a backend dropped while its socket
        // TCP send buffer was full was closed without ever telling PG.
        // PG then waited until `tcp_keepalives_idle` fired before
        // reclaiming the backend - observable as zombie idle sessions
        // after every RELOAD storm.
        //
        // The stream is held in a `ManuallyDrop` so we can take ownership
        // out of `&mut self` here and hand it to a short-lived tokio task
        // that performs a proper async `write_all + flush + shutdown`. The
        // `ManuallyDrop::take` is safe because Drop is single-shot and we
        // hold the `&mut self` exclusively; nothing else will access
        // `self.stream` after this point.
        //
        // SAFETY: `take` reads the value out of `self.stream` exactly once
        // and the field is never accessed again (drop is the last call).
        let mut stream = unsafe { std::mem::ManuallyDrop::take(&mut self.stream) };

        if !self.is_bad() {
            let mut bytes = BytesMut::with_capacity(5);
            bytes.put_u8(b'X');
            bytes.put_i32(4);

            // Fast path: try synchronous write first. If the socket has
            // room we send Terminate immediately; the spawned task then
            // only handles flush+shutdown without an additional kernel
            // round-trip.
            // capture actual bytes-written count from
            // try_write - it may return Ok(n) with n < 5 (partial
            // write). The spawned task must only write the un-sent
            // suffix or PG receives a duplicated/corrupt Terminate.
            let try_result = stream.get_mut().try_write(&bytes);
            let bytes_written = match &try_result {
                Ok(n) => *n,
                Err(_) => 0,
            };
            if let Err(ref err) = try_result {
                if err.kind() != std::io::ErrorKind::WouldBlock {
                    warn!(
                        "[{}@{}] Terminate try_write failed pid={}, scheduling async finisher: {err}",
                        self.address.username,
                        self.address.pool_name,
                        self.process_id
                    );
                }
            }
            // only `tokio::spawn` when a tokio runtime is
            // current. Otherwise (process shutdown after runtime drop,
            // sync test contexts, future refactor handling drop in a
            // catch_unwind) `tokio::spawn` panics. Falling back to a
            // synchronous close means the kernel sends FIN immediately
            // and PG observes the disconnect (without the explicit
            // Terminate frame on the WouldBlock branch) - strictly
            // better than a panic that bubbles up to the panic hook.
            let username = self.address.username.clone();
            let pool_name = self.address.pool_name.clone();
            let pid = self.process_id;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                // track in-flight graceful Terminate
                // tasks so that `binary_upgrade_and_shutdown` can wait
                // for them BEFORE `process::exit(0)`. Without this,
                // RELOAD-storm + SIGUSR2 with N hundreds of idle
                // backends silently kills almost all the spawned
                // Terminate futures mid-flight - PG observes RST/FIN
                // instead of the explicit Terminate frame, undoing
                // the graceful idle-session shutdown fix at scale.
                let terminate_guard = TerminateTaskGuard::new(
                    &IN_FLIGHT_TERMINATE_TASKS,
                    &IN_FLIGHT_TERMINATE_DRAINED,
                );
                handle.spawn(async move {
                    let _terminate_guard = terminate_guard;
                    let completed = finish_graceful_terminate(
                        stream,
                        bytes,
                        bytes_written,
                        GRACEFUL_TERMINATE_TASK_TIMEOUT,
                    )
                    .await;
                    if completed {
                        log::trace!(
                            "[{username}@{pool_name}] graceful Terminate completed pid={pid}",
                        );
                    } else {
                        warn!("[{username}@{pool_name}] graceful Terminate timed out pid={pid}");
                    }
                });
            } else {
                drop(stream);
                log::trace!(
                    "[{username}@{pool_name}] Server::Drop outside tokio runtime - \
                     falling back to synchronous close pid={pid}",
                );
            }
        } else {
            // Bad connection: drop the stream so the fd closes without
            // attempting to write - protocol state on the wire is already
            // corrupted, sending more bytes risks parser desync on the
            // PG side.
            drop(stream);
        }

        let now = chrono::offset::Utc::now().naive_utc();
        let duration = now - self.connected_at;
        let session = crate::utils::format_duration(&duration);

        match (&self.close_reason, self.bad) {
            (Some(reason), _) => info!(
                "[{}@{}] server closed pid={}: {}, session={}",
                self.address.username, self.address.pool_name, self.process_id, reason, session,
            ),
            (None, true) => info!(
                "[{}@{}] server terminated pid={}, session={}",
                self.address.username, self.address.pool_name, self.process_id, session,
            ),
            (None, false) => info!(
                "[{}@{}] server closed pid={}, session={}",
                self.address.username, self.address.pool_name, self.process_id, session,
            ),
        }
    }
}

#[cfg(test)]
impl Server {
    /// Test-only zombie `Server`: pre-marked bad, all fields at minimal
    /// defaults, backing stream is a `UnixStream` pair whose peer has
    /// been dropped. Any actual I/O attempt fails immediately with EOF
    /// (read) or `EPIPE` (write), which is exactly the state a real
    /// backend is in after PostgreSQL crashed or was killed.
    ///
    /// Intended for unit tests that need to seed a `Pool` with idle
    /// objects so `Pool::evict_dead_backends` can exercise the
    /// `is_bad()` short-circuit, the semaphore `try_acquire` /
    /// `permit.forget()` per-pop bookkeeping, the off-lock loop, and
    /// the final `size -=` / `add_permits` ordering - without spinning
    /// up a real PostgreSQL container.
    ///
    /// `Drop` is safe here because `bad = true` short-circuits the
    /// `try_write(Terminate)` branch (see `Drop::drop`).
    #[cfg(unix)]
    pub(crate) fn test_zombie_marked_bad() -> Self {
        Self::test_zombie_inner(true)
    }

    /// Test-only accessor for the backend's `process_id`. Used by
    /// `Pool::evict_dead_backends` tests to label injected zombies and
    /// then verify *which* of them survived a partial scan, which is
    /// how queue-direction (LIFO pop_back vs FIFO pop_front) is
    /// covered without needing a live PostgreSQL.
    #[cfg(test)]
    pub(crate) fn test_process_id(&self) -> i32 {
        self.process_id
    }

    /// Test-only mutator that pairs with [`test_process_id`].
    #[cfg(test)]
    pub(crate) fn test_set_process_id(&mut self, pid: i32) {
        self.process_id = pid;
    }

    /// Test-only accessor for `release_cleanup_pending`, used by tests in
    /// other modules (the field itself is private to this file).
    #[cfg(test)]
    pub(crate) fn test_release_cleanup_pending(&self) -> bool {
        self.release_cleanup_pending
    }

    /// Test-only variant of `test_zombie_marked_bad` with `bad = false`.
    /// Intended for concurrency tests that want `evict_dead_backends` to
    /// actually call `check_alive(...).await` (which then errors out and
    /// calls `mark_bad` itself), so there is a scheduling yield point
    /// per iteration of the eviction loop.
    ///
    /// `Drop` here may attempt `try_write(Terminate)` because `bad` is
    /// false at construction. The dropped peer makes the write fail
    /// quickly with EPIPE - a `warn!` line is emitted but no panic.
    #[cfg(unix)]
    pub(crate) fn test_dead_socket() -> Self {
        let (s, peer) = Self::test_zombie_inner_with_peer(false);
        drop(peer);
        s
    }

    /// Test-only variant that hands the peer `UnixStream` back to the
    /// caller instead of dropping it. The peer accepts our writes but
    /// never responds, so `check_alive(timeout)` blocks on its recv
    /// deadline. Used by cancellation-safety tests that need a
    /// deterministic await point part-way through the eviction loop -
    /// they wrap the scan in `tokio::time::timeout` and drop the
    /// future while it is parked inside `check_alive`.
    ///
    /// Caller is responsible for keeping the returned `UnixStream`
    /// alive for the duration of the scan and dropping it after.
    #[cfg(unix)]
    pub(crate) fn test_silent_socket() -> (Self, tokio::net::UnixStream) {
        Self::test_zombie_inner_with_peer(false)
    }

    #[cfg(unix)]
    fn test_zombie_inner(bad: bool) -> Self {
        let (s, peer) = Self::test_zombie_inner_with_peer(bad);
        drop(peer);
        s
    }

    #[cfg(unix)]
    fn test_zombie_inner_with_peer(bad: bool) -> (Self, tokio::net::UnixStream) {
        use dashmap::DashMap;
        use tokio::net::UnixStream;

        let (a, b) = UnixStream::pair().expect("UnixStream::pair must succeed in tests");
        let stream = BufStream::with_capacity(
            BUF_STREAM_CAPACITY,
            BUF_STREAM_CAPACITY,
            StreamInner::UnixSocket { stream: a },
        );

        let server = Server {
            address: Address::default(),
            stream: std::mem::ManuallyDrop::new(stream),
            buffer: BytesMut::new(),
            read_buf: BytesMut::new(),
            server_parameters: ServerParameters::default(),
            process_id: 0,
            secret_key: 0,
            in_transaction: false,
            command_complete_in_transaction: false,
            data_available: false,
            in_copy_mode: false,
            async_mode: false,
            expected_responses: 0,
            expected_response_sequence: VecDeque::new(),
            bad,
            internal_round_trip_in_flight: false,
            cleanup_state: CleanupState::new(),
            pending_set_cleanup_commands: VecDeque::new(),
            pending_reset_cleanup_commands: VecDeque::new(),
            pending_cleanup_disarms: PendingCleanupDisarms::default(),
            response_cycle_had_error: false,
            client_server_map: Arc::new(DashMap::new()),
            connected_at: chrono::Utc::now().naive_utc(),
            stats: Arc::new(ServerStats::default()),
            application_name: String::new(),
            last_activity: SystemTime::now(),
            last_activity_quanta: quanta::Instant::now(),
            cleanup_connections: false,
            log_client_parameter_status_changes: false,
            prepared_statement_cache: None,
            registering_prepared_statement: VecDeque::new(),
            rejected_prepared_statement_names: Vec::new(),
            has_pending_cache_entries: false,
            deferred_eviction_closes: std::collections::HashSet::new(),
            connected_with_tls: false,
            session_mode: false,
            max_message_size: 0,
            pending_large_message: None,
            close_reason: None,
            override_lifetime_ms: None,
            operator_managed_startup_keys: Arc::new(HashSet::new()),
            last_sql_error: None,
            release_query: None,
            release_cleanup_pending: false,
            intercept_discard_all: true,
        };
        (server, b)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_release_query, SetResponseOutcome, HOUSEKEEPING_TIMEOUT, RELEASE_SESSION_QUERY,
    };
    use lru::LruCache;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    #[test]
    fn release_query_absent_uses_iserv_compatible_default() {
        // Omitted `release_query` in the pool config must resolve to the
        // iServ-compatible default: pg_advisory_unlock_all + pgv_free.
        let resolved = resolve_release_query(None).expect("default release query expected");
        assert_eq!(&*resolved, RELEASE_SESSION_QUERY);
    }

    #[test]
    fn release_query_empty_disables_release() {
        // `release_query = ""` explicitly disables the release statement.
        assert!(resolve_release_query(Some("")).is_none());
    }

    #[test]
    fn release_query_custom_is_used_verbatim() {
        // Non-empty custom value is forwarded verbatim, no implicit prepending
        // or amendment to keep operator intent intact.
        let custom = "SELECT 1; RESET ALL;";
        let resolved = resolve_release_query(Some(custom)).expect("custom release query expected");
        assert_eq!(&*resolved, custom);
    }

    #[test]
    fn release_query_preencodes_the_release_only_wire_frame() {
        let resolved =
            resolve_release_query(Some("SELECT 1")).expect("custom release query should resolve");

        assert_eq!(resolved.sql(), "SELECT 1");
        assert_eq!(
            resolved.frame(),
            &crate::messages::simple_query("SELECT 1;")[..]
        );
    }

    #[test]
    fn release_only_checkin_uses_the_preencoded_frame() {
        let src = include_str!("server_backend.rs");
        let impl_src = src;

        let finalize_start = impl_src
            .find("async fn finalize_checkin_inner")
            .expect("finalize_checkin_inner not found");
        let finalize = &impl_src[finalize_start..];
        let release_only_call = finalize
            .find("self.send_release_query_only(release_query).await")
            .expect("release-only fast path not found");
        let combined_push = finalize
            .find("stmts.push(release_query.sql().to_string())")
            .expect("combined cleanup path not found");
        assert!(release_only_call < combined_push);

        let helper_start = impl_src
            .find("async fn send_release_query_only")
            .expect("release-only helper not found");
        let helper = &impl_src[helper_start..];
        assert!(
            helper.contains("small_simple_query_frame(release_query.frame())"),
            "release-only check-in must send the pool-shared frame directly"
        );
    }

    #[test]
    fn backend_startup_body_reads_use_memory_budget() {
        let src = include_str!("server_backend.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        assert!(
            !impl_src.contains("read_message_data(&mut stream"),
            "backend startup must not read direct message bodies without memory-budget reservation"
        );
        assert!(
            impl_src.contains("let max_memory_usage = config.general.max_memory_usage.as_bytes()"),
            "backend startup must capture the configured message memory budget"
        );
        assert!(
            impl_src.matches("read_message_data_with_memory_limit(").count() >= 4,
            "startup ErrorResponse/Notice/ParameterStatus/ReadyForQuery bodies must use budgeted reads"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_server_data_completes_for_response_already_buffered_in_bufstream() {
        use super::Server;
        use bytes::{BufMut, BytesMut};
        use tokio::io::AsyncWriteExt;

        fn command_complete(tag: &str) -> BytesMut {
            let mut m = BytesMut::new();
            m.put_u8(b'C');
            m.put_i32(4 + tag.len() as i32 + 1);
            m.put_slice(tag.as_bytes());
            m.put_u8(0);
            m
        }
        fn ready_for_query() -> BytesMut {
            let mut m = BytesMut::new();
            m.put_u8(b'Z');
            m.put_i32(5);
            m.put_u8(b'I');
            m
        }

        let (mut server, mut peer) = Server::test_silent_socket();

        // Two pipelined simple-query responses delivered in ONE write, the
        // way a piggybacked `SET application_name` + client query lands when
        // the kernel coalesces both replies into a single segment.
        let mut frames = BytesMut::new();
        frames.put(&command_complete("SET")[..]);
        frames.put(&ready_for_query()[..]);
        frames.put(&command_complete("SELECT 1")[..]);
        frames.put(&ready_for_query()[..]);
        peer.write_all(&frames).await.expect("peer write");
        peer.flush().await.expect("peer flush");

        // The first recv() stops at the first ReadyForQuery but has already
        // pulled BOTH responses into the BufStream userspace buffer.
        let first = server
            .recv(&mut tokio::io::sink(), None)
            .await
            .expect("first response");
        assert!(first.ends_with(&[b'Z', 0, 0, 0, 5, b'I']));
        assert!(!server.is_data_available());

        // The raw socket is drained: readiness-based waiting can never fire
        // again, even though a full response is sitting in the buffer. This
        // is the deadlock the piggyback path hit.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.server_readable())
                .await
                .is_err(),
            "raw-socket readable() must NOT see bytes buffered inside BufStream"
        );

        // wait_server_data() must complete immediately from the buffered
        // bytes, and recv() must then deliver the second response.
        tokio::time::timeout(Duration::from_millis(500), server.wait_server_data())
            .await
            .expect("buffered response bytes must complete the server-data wait");
        let second = server
            .recv(&mut tokio::io::sink(), None)
            .await
            .expect("second response");
        assert!(second.ends_with(&[b'Z', 0, 0, 0, 5, b'I']));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_piggyback_set_preserves_following_client_response() {
        use super::Server;
        use bytes::{BufMut, BytesMut};
        use tokio::io::AsyncWriteExt;

        fn error_response(sqlstate: &str, message: &str) -> BytesMut {
            let mut body = BytesMut::new();
            body.put_u8(b'S');
            body.put_slice(b"ERROR\0");
            body.put_u8(b'C');
            body.put_slice(sqlstate.as_bytes());
            body.put_u8(0);
            body.put_u8(b'M');
            body.put_slice(message.as_bytes());
            body.put_u8(0);
            body.put_u8(0);

            let mut frame = BytesMut::new();
            frame.put_u8(b'E');
            frame.put_i32(4 + body.len() as i32);
            frame.put(body);
            frame
        }

        fn command_complete(tag: &str) -> BytesMut {
            let mut frame = BytesMut::new();
            frame.put_u8(b'C');
            frame.put_i32(4 + tag.len() as i32 + 1);
            frame.put_slice(tag.as_bytes());
            frame.put_u8(0);
            frame
        }

        fn ready_for_query() -> BytesMut {
            let mut frame = BytesMut::new();
            frame.put_u8(b'Z');
            frame.put_i32(5);
            frame.put_u8(b'I');
            frame
        }

        let (mut server, mut peer) = Server::test_silent_socket();
        let mut frames = BytesMut::new();
        frames.put(&error_response("57014", "canceling statement due to user request")[..]);
        frames.put(&ready_for_query()[..]);
        frames.put(&command_complete("SELECT 1")[..]);
        frames.put(&ready_for_query()[..]);
        peer.write_all(&frames).await.expect("peer write");
        peer.flush().await.expect("peer flush");

        let outcome = server
            .swallow_set_response()
            .await
            .expect("SQL rejection is a protocol-complete response");
        assert_eq!(
            outcome,
            SetResponseOutcome::Rejected {
                sqlstate: "57014".to_string(),
                message: "canceling statement due to user request".to_string(),
            }
        );
        assert!(!server.is_bad());

        let client_response = server
            .recv(&mut tokio::io::sink(), None)
            .await
            .expect("following client response");
        assert!(client_response.starts_with(b"C"));
        assert!(client_response.ends_with(&[b'Z', 0, 0, 0, 5, b'I']));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn reissues_cancel_only_for_a_quarantined_backend() {
        use super::Server;
        use crate::pool::CANCELED_PIDS;
        use std::time::Instant;
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind cancel listener");
        let port = listener.local_addr().expect("listener address").port();

        let (mut server, _peer) = Server::test_silent_socket();
        server.address.host = "127.0.0.1".to_string();
        server.address.port = port;
        server.process_id = -901_221;
        server.secret_key = 0x1020_3040;

        assert!(server.reissue_cancel_if_marked().await.is_none());

        CANCELED_PIDS.insert(server.process_id, Instant::now());
        let receive = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept cancel connection");
            let mut frame = [0_u8; 16];
            socket
                .read_exact(&mut frame)
                .await
                .expect("read cancel frame");
            frame
        });

        server
            .reissue_cancel_if_marked()
            .await
            .expect("cancel marker")
            .expect("reissued cancel");
        let frame = receive.await.expect("cancel receiver task");
        assert_eq!(i32::from_be_bytes(frame[0..4].try_into().unwrap()), 16);
        assert_eq!(
            i32::from_be_bytes(frame[8..12].try_into().unwrap()),
            server.process_id
        );
        assert_eq!(
            i32::from_be_bytes(frame[12..16].try_into().unwrap()),
            server.secret_key
        );
        CANCELED_PIDS.remove(&server.process_id);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clear_internal_set_cleanup_state_disarms_set_cleanup() {
        use super::Server;
        use crate::server::cleanup::SetCleanupCommand;

        let (mut server, _peer) = Server::test_silent_socket();
        server.cleanup_state.needs_cleanup_set = true;
        server.track_set_cleanup_commands([SetCleanupCommand::GenericSet]);

        server.clear_internal_set_cleanup_state();

        assert!(
            !server.cleanup_state.needs_cleanup_set,
            "internal pooler SET cleanup must disarm the checkin RESET ALL flag"
        );
        assert!(
            server.pop_set_cleanup_command().is_none(),
            "internal pooler SET cleanup must also clear queued attribution"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finalize_checkin_rejects_release_query_that_leaves_transaction_open() {
        use super::Server;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut server, mut peer) = Server::test_silent_socket();
        server.set_release_query(Some("BEGIN"));

        let peer_task = tokio::spawn(async move {
            let mut header = [0_u8; 5];
            peer.read_exact(&mut header)
                .await
                .expect("peer must receive simple Query header");
            assert_eq!(header[0], b'Q');
            let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; (len - 4) as usize];
            peer.read_exact(&mut body)
                .await
                .expect("peer must receive simple Query body");
            assert_eq!(&body, b"BEGIN;\0");

            peer.write_all(&[
                b'C', 0, 0, 0, 10, b'B', b'E', b'G', b'I', b'N', 0, b'Z', 0, 0, 0, 5, b'T',
            ])
            .await
            .expect("peer must write CommandComplete + ReadyForQuery(T)");
        });

        let result = server.finalize_checkin().await;
        peer_task.await.expect("peer task must finish");

        assert!(
            result.is_err(),
            "release_query that leaves PostgreSQL inside a transaction must not be accepted"
        );
        assert!(
            server.is_bad(),
            "non-idle backend must be marked bad before it can return to the pool"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finalize_checkin_rejects_release_query_that_leaves_cleanup_dirty() {
        use super::Server;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut server, mut peer) = Server::test_silent_socket();
        server.cleanup_connections = true;
        server.cleanup_state.needs_cleanup_set = true;
        server.set_release_query(Some("SET client.app_user = 'release'"));

        let peer_task = tokio::spawn(async move {
            let mut header = [0_u8; 5];
            peer.read_exact(&mut header)
                .await
                .expect("peer must receive simple Query header");
            assert_eq!(header[0], b'Q');
            let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; (len - 4) as usize];
            peer.read_exact(&mut body)
                .await
                .expect("peer must receive simple Query body");
            assert_eq!(
                &body,
                b"RESET ROLE;\nRESET ALL;\nSET client.app_user = 'release';\0"
            );

            peer.write_all(&[
                b'C', 0, 0, 0, 10, b'R', b'E', b'S', b'E', b'T', 0, b'C', 0, 0, 0, 10, b'R', b'E',
                b'S', b'E', b'T', 0, b'C', 0, 0, 0, 8, b'S', b'E', b'T', 0, b'Z', 0, 0, 0, 5, b'I',
            ])
            .await
            .expect("peer must write cleanup RESETs + release SET + ReadyForQuery");
        });

        let result = server.finalize_checkin().await;
        peer_task.await.expect("peer task must finish");

        assert!(
            result.is_err(),
            "release_query that leaves cleanup_state dirty must not be accepted"
        );
        assert!(
            server.is_bad(),
            "dirty release_query must make the backend non-reusable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_finalize_clears_release_cleanup_pending() {
        use super::Server;
        use crate::web::metrics::CHECKIN_CLEANUP_SECONDS;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut server, mut peer) = Server::test_silent_socket();
        server.address.username = "release_metric_user".to_string();
        server.address.database = "release_metric_database".to_string();
        server.set_release_query(Some("SELECT pg_advisory_unlock_all()"));
        server.arm_release_cleanup();
        assert!(server.release_cleanup_pending);
        let metric_user = server.address.username.clone();
        let metric_database = server.address.database.clone();
        let labels = [
            metric_user.as_str(),
            metric_database.as_str(),
            "release_only",
            "ok",
        ];
        let metric_before = CHECKIN_CLEANUP_SECONDS
            .with_label_values(&labels)
            .get_sample_count();

        let peer_task = tokio::spawn(async move {
            let mut header = [0_u8; 5];
            peer.read_exact(&mut header)
                .await
                .expect("peer must receive simple Query header");
            assert_eq!(header[0], b'Q');
            let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; (len - 4) as usize];
            peer.read_exact(&mut body)
                .await
                .expect("peer must receive simple Query body");
            assert_eq!(&body, b"SELECT pg_advisory_unlock_all();\0");

            peer.write_all(&[
                b'C', 0, 0, 0, 13, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1', 0, b'Z', 0, 0,
                0, 5, b'I',
            ])
            .await
            .expect("peer must write CommandComplete + ReadyForQuery(I)");
        });

        server
            .finalize_checkin()
            .await
            .expect("release query round trip should succeed");
        peer_task.await.expect("peer task must finish");

        assert!(
            !server.release_cleanup_pending,
            "a confirmed release round trip must disarm the pending flag"
        );
        assert!(!server.is_bad());
        assert_eq!(
            CHECKIN_CLEANUP_SECONDS
                .with_label_values(&labels)
                .get_sample_count(),
            metric_before + 1,
            "successful release-only check-in must be observable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_finalize_keeps_release_cleanup_pending() {
        use super::Server;

        // Dead peer: sending the release query fails with a transport error.
        let mut server = Server::test_dead_socket();
        server.set_release_query(Some("SELECT pg_advisory_unlock_all()"));
        server.arm_release_cleanup();

        let result = server.finalize_checkin().await;

        assert!(
            result.is_err(),
            "release query over a dead socket must surface as Err"
        );
        assert!(
            server.release_cleanup_pending,
            "an unconfirmed release round trip must keep the pending flag armed"
        );
        assert!(
            server.is_bad(),
            "the failed release round trip must make the backend non-reusable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plain_checkin_cleanup_does_not_clear_release_cleanup_pending() {
        use super::Server;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut server, mut peer) = Server::test_silent_socket();
        server.cleanup_connections = true;
        server.cleanup_state.needs_cleanup_set = true;
        server.set_release_query(Some("SELECT pg_advisory_unlock_all()"));
        server.arm_release_cleanup();

        let peer_task = tokio::spawn(async move {
            let mut header = [0_u8; 5];
            peer.read_exact(&mut header)
                .await
                .expect("peer must receive simple Query header");
            assert_eq!(header[0], b'Q');
            let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; (len - 4) as usize];
            peer.read_exact(&mut body)
                .await
                .expect("peer must receive simple Query body");
            // The checkout-side cleanup never appends the release query.
            assert_eq!(&body, b"RESET ROLE;\nRESET ALL;\0");

            peer.write_all(&[
                b'C', 0, 0, 0, 10, b'R', b'E', b'S', b'E', b'T', 0, b'C', 0, 0, 0, 10, b'R', b'E',
                b'S', b'E', b'T', 0, b'Z', 0, 0, 0, 5, b'I',
            ])
            .await
            .expect("peer must write cleanup RESETs + ReadyForQuery");
        });

        server
            .checkin_cleanup()
            .await
            .expect("internal cleanup should succeed");
        peer_task.await.expect("peer task must finish");

        assert!(
            server.release_cleanup_pending,
            "checkout-side checkin_cleanup must not disarm the release obligation"
        );
        assert!(!server.is_bad());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checkin_cleanup_reset_all_clears_startup_only_parameter_mirror() {
        use super::Server;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut server, mut peer) = Server::test_silent_socket();
        server.cleanup_connections = true;
        server.cleanup_state.needs_cleanup_set = true;
        server
            .server_parameters
            .set_param("client.app_user", "alice", true);

        let peer_task = tokio::spawn(async move {
            let mut header = [0_u8; 5];
            peer.read_exact(&mut header)
                .await
                .expect("peer must receive simple Query header");
            assert_eq!(header[0], b'Q');
            let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; (len - 4) as usize];
            peer.read_exact(&mut body)
                .await
                .expect("peer must receive simple Query body");
            assert_eq!(&body, b"RESET ROLE;\nRESET ALL;\0");

            peer.write_all(&[
                b'C', 0, 0, 0, 10, b'R', b'E', b'S', b'E', b'T', 0, b'C', 0, 0, 0, 10, b'R', b'E',
                b'S', b'E', b'T', 0, b'Z', 0, 0, 0, 5, b'I',
            ])
            .await
            .expect("peer must write cleanup RESETs + ReadyForQuery");
        });

        server
            .checkin_cleanup()
            .await
            .expect("internal cleanup should succeed");
        peer_task.await.expect("peer task must finish");

        assert!(
            !server
                .server_parameters
                .as_hashmap()
                .contains_key("client.app_user"),
            "successful internal RESET ALL must clear startup-only server parameter mirrors"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_checkin_rollback_reset_all_clears_startup_only_parameter_mirror() {
        use super::Server;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut server, mut peer) = Server::test_silent_socket();
        server.cleanup_connections = true;
        server.in_transaction = true;
        server.cleanup_state.needs_cleanup_set = true;
        server
            .server_parameters
            .set_param("search_path", "tenant_a", true);

        let peer_task = tokio::spawn(async move {
            let mut header = [0_u8; 5];
            peer.read_exact(&mut header)
                .await
                .expect("peer must receive simple Query header");
            assert_eq!(header[0], b'Q');
            let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; (len - 4) as usize];
            peer.read_exact(&mut body)
                .await
                .expect("peer must receive simple Query body");
            assert_eq!(&body, b"ROLLBACK;\nRESET ROLE;\nRESET ALL;\0");

            peer.write_all(&[
                b'C', 0, 0, 0, 13, b'R', b'O', b'L', b'L', b'B', b'A', b'C', b'K', 0, b'C', 0, 0,
                0, 10, b'R', b'E', b'S', b'E', b'T', 0, b'C', 0, 0, 0, 10, b'R', b'E', b'S', b'E',
                b'T', 0, b'Z', 0, 0, 0, 5, b'I',
            ])
            .await
            .expect("peer must write rollback, cleanup RESETs, and ReadyForQuery");
        });

        server
            .checkin_cleanup()
            .await
            .expect("forced internal cleanup should succeed");
        peer_task.await.expect("peer task must finish");

        assert!(
            !server
                .server_parameters
                .as_hashmap()
                .contains_key("search_path"),
            "successful forced cleanup RESET ALL must clear startup-only mirrors \
             even though CommandComplete(RESET) arrived before ReadyForQuery(I)"
        );
    }

    #[test]
    fn housekeeping_timeout_matches_iserv_baseline() {
        // The 30-second window is sized for production checkin latencies
        // observed in the iServ deployment. Bumping it silently would
        // increase the worst-case checkin delay when PostgreSQL gets stuck.
        // Lowering it would cause false-positive bad-marks under load.
        // Treat this as a guard, not a knob.
        assert_eq!(HOUSEKEEPING_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn combined_sql_preview_truncates_on_utf8_boundary() {
        let combined = format!("{}ы", "a".repeat(199));

        assert_eq!(
            super::combined_sql_preview(&combined, 200),
            format!("{}…(+2B)", "a".repeat(199)),
        );
    }

    #[test]
    fn sql_string_literal_simple_value() {
        // Plain ASCII without any `$` runs picks tag 0 and wraps verbatim.
        assert_eq!(
            super::sql_string_literal("UTC").unwrap(),
            "$pgdoorman0$UTC$pgdoorman0$"
        );
    }

    #[test]
    fn sql_string_literal_with_single_quotes_does_not_need_escaping() {
        // The parser bug: single-quote SQL literals required `''`
        // escaping. Dollar-quoting passes apostrophes verbatim, so a
        // value like `O'Brien` no longer triggers a syntax error.
        let literal = super::sql_string_literal("O'Brien").unwrap();
        assert_eq!(literal, "$pgdoorman0$O'Brien$pgdoorman0$");
    }

    #[test]
    fn sql_string_literal_handles_backslashes_and_quotes() {
        // standard_conforming_strings = on or not, PostgreSQL never treats
        // `\` inside a dollar-quoted literal as an escape. A pathological
        // search_path entry with both kinds of quotes stays intact.
        let literal = super::sql_string_literal(r#"a"b'c\d"#).unwrap();
        assert_eq!(literal, "$pgdoorman0$a\"b'c\\d$pgdoorman0$");
    }

    #[test]
    fn sql_string_literal_avoids_tag_collision() {
        // If the value itself contains `$pgdoorman0$`, we must pick the next
        // numbered tag so the closing delimiter is unambiguous.
        let collide = "evil $pgdoorman0$ value";
        let literal = super::sql_string_literal(collide).unwrap();
        assert_eq!(literal, "$pgdoorman1$evil $pgdoorman0$ value$pgdoorman1$");
    }

    #[test]
    fn sql_string_literal_avoids_multi_tag_collision() {
        // Value contains both $pgdoorman0$ and $pgdoorman1$; helper must
        // bump until it finds a free tag. Tag 2 is the first untouched one.
        let collide = "$pgdoorman0$ and $pgdoorman1$";
        let literal = super::sql_string_literal(collide).unwrap();
        assert_eq!(
            literal,
            "$pgdoorman2$$pgdoorman0$ and $pgdoorman1$$pgdoorman2$"
        );
    }

    #[test]
    fn sql_string_literal_avoids_boundary_overlap_at_suffix() {
        // regression: value ending with `$pgdoorman<N>` (no trailing
        // `$`) used to pass the old `!value.contains(tag)` check because the
        // suffix is missing the closing `$`. After concatenation, the
        // value's suffix combines with the opening `$` of the next tag to
        // form a premature `$pgdoorman<N>$` match - PostgreSQL closes the
        // literal early and the remainder parses as garbage tokens.
        //
        // Realistic DoS shape: a client sets `application_name =
        // "x$pgdoorman0"`. The old impl produces
        // `$pgdoorman0$x$pgdoorman0$pgdoorman0$` which PG parses as
        // body=`x` and trailing junk `pgdoorman0$;` -> SQLSTATE 42601.
        // `sync_parameters` would then mark every backend bad on each
        // per-client startup, churning the pool indefinitely.
        let exploit = "x$pgdoorman0";
        let literal = super::sql_string_literal(exploit).unwrap();
        // Helper must bump to tag 1 so the result has exactly two tag
        // occurrences and the body parses cleanly as the original value.
        assert_eq!(literal, "$pgdoorman1$x$pgdoorman0$pgdoorman1$");
    }

    #[test]
    fn sql_string_literal_safe_when_value_starts_with_tag_body() {
        // Documents the asymmetric boundary case: a value beginning with
        // `pgdoorman<N>$` is benign because there is no leading `$` to
        // combine with the opening tag's trailing `$` and form a
        // premature closing match. The boundary-overlap problem only
        // manifests at the SUFFIX (value ending with `$pgdoorman<N>` and
        // no trailing `$`). Pinning this so a future "defensive"
        // tightening doesn't reject benign prefix shapes and force
        // unnecessary tag bumps.
        let value = "pgdoorman0$y";
        let literal = super::sql_string_literal(value).unwrap();
        assert_eq!(literal, "$pgdoorman0$pgdoorman0$y$pgdoorman0$");
    }

    #[test]
    fn sql_string_literal_bumps_past_multiple_overlapping_suffixes() {
        // Adversarial chain: value carrying suffix `$pgdoorman0` AND
        // containing `$pgdoorman1$` would force the helper to skip both
        // tag 0 (suffix overlap) and tag 1 (in-body collision), landing
        // on tag 2.
        let value = "abc $pgdoorman1$ middle $pgdoorman0";
        let literal = super::sql_string_literal(value).unwrap();
        assert_eq!(
            literal,
            "$pgdoorman2$abc $pgdoorman1$ middle $pgdoorman0$pgdoorman2$"
        );
    }

    #[test]
    fn sql_string_literal_benign_values_stay_on_tag_zero() {
        // Performance / nostalgia: the common case is a plain GUC value
        // with no `$` anywhere, which must keep using `$pgdoorman0$` on
        // the first iteration (no loop bump) so the per-checkout
        // `sync_parameters` allocation stays at one tag attempt.
        assert_eq!(
            super::sql_string_literal("UTC").unwrap(),
            "$pgdoorman0$UTC$pgdoorman0$"
        );
        assert_eq!(
            super::sql_string_literal("").unwrap(),
            "$pgdoorman0$$pgdoorman0$"
        );
        assert_eq!(
            super::sql_string_literal("O'Brien").unwrap(),
            "$pgdoorman0$O'Brien$pgdoorman0$"
        );
    }

    /// Regression guard: `collect_checkin_cleanup_sqls`
    /// must `mark_bad` and `Err` when the prepared-statement cache is
    /// inconsistent with the server's view AND `cleanup_server_connections`
    /// is disabled, because `DEALLOCATE ALL` cannot be sent to bring the
    /// two sides back in sync.
    ///
    /// Without this guard, a backend with `has_pending_cache_entries =
    /// true` (or `deferred_eviction_closes` non-empty, or any other
    /// path that sets `cleanup_state.needs_cleanup_prepare`) would be
    /// silently returned to the idle queue with a server-side prepared
    /// statement that pg_doorman believes was wiped - the next client
    /// `Parse` on the same deterministic `DOORMAN_N` name would then
    /// hit SQLSTATE 42P05 ("prepared statement already exists").
    ///
    /// This was a latent bug in the upstream code path (the cleanup
    /// block was simply skipped when `cleanup_connections=false`, the
    /// cache mismatch was left silent); the recent pipelining refactor
    /// in this branch made it more catchable because the cache-clear
    /// path was briefly hoisted out of the gate. Both shapes are now
    /// addressed by the same `mark_bad` + `Err` contract.
    #[cfg(unix)]
    #[tokio::test]
    async fn collect_checkin_cleanup_marks_bad_on_prepared_mismatch_with_cleanup_disabled() {
        use super::Server;

        // test_dead_socket builds a Server with bad=false and
        // cleanup_connections=false by construction, perfect for this
        // scenario: pre-state checks (in_copy_mode / data_available /
        // empty buffer) all pass on the default, so the promotion +
        // mismatch gate is the first thing collect_* reaches.
        let mut server = Server::test_dead_socket();
        assert!(
            !server.is_bad(),
            "test precondition: backend starts clean (bad=false)"
        );
        assert!(
            !server.cleanup_connections,
            "test precondition: cleanup_connections must be false to \
             trigger the mismatch path"
        );

        // Simulate the scenario: client added a Parse to pg_doorman's
        // local cache but disconnected before the Sync that would have
        // flushed it to PostgreSQL. The server side never registered
        // the statement, but pg_doorman's bookkeeping thinks it did.
        server.has_pending_cache_entries = true;

        let result = server.collect_checkin_cleanup_sqls();
        assert!(
            result.is_err(),
            "collect_checkin_cleanup_sqls must Err when prepared cache \
             mismatch cannot be reconciled (got Ok({:?}))",
            result.ok(),
        );
        assert!(
            server.is_bad(),
            "Server must be marked bad so the pool drops it on Drop \
             instead of returning it to the idle queue with an \
             inconsistent server-side prepared cache",
        );

        // And the same shape via `deferred_eviction_closes` (the other
        // promotion source) - sanity check that the mark_bad path is
        // not specific to one promotion trigger.
        let mut server2 = Server::test_dead_socket();
        server2
            .deferred_eviction_closes
            .insert("DOORMAN_42".to_string());
        let result2 = server2.collect_checkin_cleanup_sqls();
        assert!(
            result2.is_err(),
            "deferred_eviction_closes promotion must also Err on \
             cleanup-disabled mismatch"
        );
        assert!(server2.is_bad());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn collect_checkin_cleanup_rejects_pending_large_message() {
        use super::Server;

        let mut server = Server::test_dead_socket();
        server.pending_large_message = Some((b'D', 128));

        let result = server.collect_checkin_cleanup_sqls();
        assert!(
            result.is_err(),
            "checkin cleanup must reject a backend after a large-frame header \
             has been consumed but before the body has been drained"
        );
        assert!(
            server.is_bad(),
            "pending large-frame state must mark the backend bad so the pool \
             closes it instead of returning unread body bytes to another client"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recycle_safety_violation_flags_states_that_need_async_cleanup_or_drain() {
        use super::Server;
        use crate::server::cleanup::ResetCleanupCommand;

        let clean = Server::test_dead_socket();
        assert_eq!(
            clean.recycle_safety_violation(),
            None,
            "a newly-created test backend with no protocol/session state is reusable"
        );

        type DirtyBackendCase = (&'static str, fn(&mut Server), &'static str);
        let cases: &[DirtyBackendCase] = &[
            (
                "pending large",
                |s| s.pending_large_message = Some((b'D', 128)),
                "returned with pending large frame",
            ),
            (
                "copy mode",
                |s| s.in_copy_mode = true,
                "returned in copy-mode",
            ),
            (
                "data available",
                |s| s.data_available = true,
                "returned with data available",
            ),
            (
                "async mode",
                |s| s.set_async_mode(true),
                "returned in async protocol mode",
            ),
            (
                "buffer",
                |s| s.buffer.extend_from_slice(b"x"),
                "returned with not-empty buffer",
            ),
            (
                "internal round trip",
                |s| s.begin_internal_round_trip(),
                "returned with internal round trip in flight",
            ),
            (
                "transaction",
                |s| s.in_transaction = true,
                "returned in transaction",
            ),
            (
                "cleanup state",
                |s| s.cleanup_state.needs_cleanup_set = true,
                "returned with dirty session state",
            ),
            (
                "pending cache",
                |s| s.has_pending_cache_entries = true,
                "returned with pending prepared cache entries",
            ),
            (
                "deferred closes",
                |s| {
                    s.deferred_eviction_closes.insert("DOORMAN_1".to_string());
                },
                "returned with deferred prepared-statement closes",
            ),
            (
                "pending reset attribution",
                |s| {
                    s.pending_reset_cleanup_commands
                        .push_back(ResetCleanupCommand::PerGucReset)
                },
                "returned with pending reset attribution",
            ),
            (
                "pending release cleanup",
                |s| {
                    s.set_release_query(None);
                    s.arm_release_cleanup();
                },
                "returned without successful release cleanup",
            ),
        ];

        for (label, dirty, expected) in cases {
            let mut server = Server::test_dead_socket();
            dirty(&mut server);
            assert_eq!(
                server.recycle_safety_violation(),
                Some(*expected),
                "{label} must prevent synchronous Drop recycling"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn collect_checkin_cleanup_rejects_async_backend_without_ready_for_query() {
        use super::Server;

        let mut server = Server::test_dead_socket();
        server.set_async_mode(true);
        server.set_expected_responses(0);

        let result = server.collect_checkin_cleanup_sqls();

        assert!(
            result.is_err(),
            "async Flush backends must not be returned to the pool until a \
             ReadyForQuery has reconciled transaction state"
        );
        assert!(
            server.is_bad(),
            "async backend without ReadyForQuery must be marked bad so it is evicted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn collect_checkin_cleanup_resets_session_authorization_before_reuse() {
        use super::Server;

        let mut server = Server::test_dead_socket();
        server.cleanup_connections = true;
        server.cleanup_state.needs_cleanup_session_authorization = true;

        let stmts = server
            .collect_checkin_cleanup_sqls()
            .expect("session authorization cleanup must be representable as SQL");

        assert!(
            stmts
                .iter()
                .any(|stmt| stmt == "RESET SESSION AUTHORIZATION"),
            "dirty session authorization must be explicitly reset; RESET ALL is not enough"
        );
        assert!(
            stmts.iter().any(|stmt| stmt == "RESET ROLE"),
            "session authorization cleanup should also normalize current role"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canceling_small_simple_query_after_send_evicts_backend() {
        use super::Server;
        use tokio::io::AsyncReadExt;

        let (mut server, mut peer) = Server::test_silent_socket();

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            server.small_simple_query("RESET ALL"),
        )
        .await;
        assert!(
            result.is_err(),
            "silent peer must keep small_simple_query pending until the outer timeout cancels it"
        );

        let mut observed = [0_u8; 64];
        let n = tokio::time::timeout(Duration::from_millis(50), peer.read(&mut observed))
            .await
            .expect("test peer must observe the housekeeping write before cancellation")
            .expect("reading from test peer must succeed");
        assert!(
            n > 0,
            "test precondition: housekeeping bytes must have reached the peer"
        );

        assert!(
            server.is_bad(),
            "cancelling a housekeeping query after bytes were sent must make the backend non-reusable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canceling_deferred_eviction_close_after_send_evicts_backend() {
        use super::Server;
        use tokio::io::AsyncReadExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.queue_deferred_eviction_close("DOORMAN_1".to_string());

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            server.send_deferred_eviction_closes(),
        )
        .await;
        assert!(
            result.is_err(),
            "silent peer must keep deferred Close drain pending until the outer timeout cancels it"
        );

        let mut observed = [0_u8; 128];
        let n = tokio::time::timeout(Duration::from_millis(50), peer.read(&mut observed))
            .await
            .expect("test peer must observe the deferred Close write before cancellation")
            .expect("reading from test peer must succeed");
        assert!(
            n > 0,
            "test precondition: deferred Close bytes must have reached the peer"
        );

        assert!(
            server.is_bad(),
            "cancelling deferred Close drain after bytes were sent must make the backend non-reusable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_eviction_close_defers_while_in_copy_mode() {
        use super::Server;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut server, mut peer) = Server::test_silent_socket();
        server.queue_deferred_eviction_close("DOORMAN_1".to_string());
        server.in_copy_mode = true;

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            server.send_deferred_eviction_closes(),
        )
        .await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "COPY-mode deferred close must return without writing Close+Sync"
        );
        assert!(
            server.deferred_eviction_closes.contains("DOORMAN_1"),
            "deferred close must remain queued until COPY finishes"
        );

        let mut observed = [0_u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read(&mut observed))
                .await
                .is_err(),
            "peer must not receive Close+Sync while backend is in COPY mode"
        );

        server.in_copy_mode = false;
        let peer_task = tokio::spawn(async move {
            let mut observed = [0_u8; 128];
            let n = tokio::time::timeout(Duration::from_millis(50), peer.read(&mut observed))
                .await
                .expect("peer must observe deferred Close+Sync after COPY exits")
                .expect("reading deferred Close+Sync must succeed");
            assert!(n > 0, "deferred Close+Sync must reach peer");
            peer.write_all(&[b'3', 0, 0, 0, 4, b'Z', 0, 0, 0, 5, b'I'])
                .await
                .expect("peer must write CloseComplete + ReadyForQuery");
        });

        server
            .send_deferred_eviction_closes()
            .await
            .expect("deferred close must drain after COPY exits");
        peer_task.await.expect("peer task must finish");
        assert!(
            server.deferred_eviction_closes.is_empty(),
            "deferred close queue must drain after COPY exits"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_eviction_close_defers_while_flush_async_is_open() {
        use super::Server;
        use tokio::io::AsyncReadExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.queue_deferred_eviction_close("DOORMAN_1".to_string());
        server.set_async_mode(true);
        server.set_expected_responses(0);

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            server.send_deferred_eviction_closes(),
        )
        .await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "Flush-mode deferred close must wait for the client Sync instead of injecting Close+Sync"
        );
        assert!(
            server.deferred_eviction_closes.contains("DOORMAN_1"),
            "deferred close must remain queued while the frontend extended-protocol cycle is async"
        );

        let mut observed = [0_u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read(&mut observed))
                .await
                .is_err(),
            "peer must not receive backend-only Close+Sync while client Sync is still pending"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn register_prepared_statement_times_out_on_silent_backend() {
        use super::Server;
        use crate::messages::extended::Parse;
        use tokio::io::AsyncReadExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(4).unwrap(),
            ahash::RandomState::new(),
        ));
        let parse = Parse::from_parts("SELECT 1", &[]);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            server.register_prepared_statement_with_timeout(
                &parse,
                "DOORMAN_1",
                true,
                Duration::from_millis(50),
            ),
        )
        .await;
        assert!(
            outcome.is_ok(),
            "register_prepared_statement must return on its own deadline, not hang"
        );
        let result = outcome.expect("did not hang");
        assert!(
            result.is_err(),
            "silent peer must fail the bounded Parse+Sync round-trip"
        );

        let mut observed = [0_u8; 128];
        let n = tokio::time::timeout(Duration::from_millis(50), peer.read(&mut observed))
            .await
            .expect("test peer must observe the Parse+Sync write before cancellation")
            .expect("reading from test peer must succeed");
        assert!(n > 0, "test precondition: Parse+Sync bytes must reach peer");

        assert!(
            server.is_bad(),
            "a stalled server-side reprepare must make the backend non-reusable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn register_prepared_statement_rejects_hidden_sync_while_flush_async_is_open() {
        use super::Server;
        use crate::messages::extended::Parse;
        use tokio::io::AsyncReadExt;

        let (mut server, mut peer) = Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(4).unwrap(),
            ahash::RandomState::new(),
        ));
        server.set_async_mode(true);
        server.set_expected_responses(0);
        let parse = Parse::from_parts("SELECT 1", &[]);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            server.register_prepared_statement_with_timeout(
                &parse,
                "DOORMAN_1",
                true,
                Duration::from_millis(50),
            ),
        )
        .await;
        assert!(
            matches!(outcome, Ok(Err(_))),
            "server-side reprepare during Flush async mode must fail closed"
        );
        assert!(
            server.is_bad(),
            "backend must not be reused after an impossible async reprepare"
        );
        assert!(
            server.is_async(),
            "rejecting the hidden Sync must not silently reconcile the frontend protocol cycle"
        );

        let mut observed = [0_u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read(&mut observed))
                .await
                .is_err(),
            "peer must not receive backend-only Parse+Sync while client Sync is still pending"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_available_recv_error_marks_backend_bad() {
        use super::Server;

        let mut server = Server::test_dead_socket();
        server.data_available = true;

        server.wait_available().await;

        assert!(
            server.is_bad(),
            "failed wait_available drain must make the backend non-reusable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canceling_wait_available_drain_marks_backend_non_reusable() {
        use super::Server;

        let (mut server, _peer) = Server::test_silent_socket();
        server.data_available = true;

        let result = tokio::time::timeout(Duration::from_millis(50), server.wait_available()).await;
        assert!(
            result.is_err(),
            "silent peer must keep wait_available pending until the outer timeout cancels it"
        );
        assert!(
            server.is_bad(),
            "cancelling wait_available while unread data is being drained must make the backend non-reusable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_available_times_out_on_silent_backend() {
        // a backend that stalls mid-drain
        // on a live-but-silent socket must be bounded by the drain deadline
        // and marked bad WITHOUT any external cancellation. Drive the inner
        // with a short deadline; it must return on its own (the outer 5s guard
        // turns a regression - missing internal timeout - into a failure
        // instead of a hung suite) and leave the backend non-reusable.
        use super::Server;
        use std::time::Duration;

        let (mut server, _peer) = Server::test_silent_socket();
        server.data_available = true;

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            server.wait_available_with_deadline(Duration::from_millis(50)),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "wait_available must return on its own drain deadline, not hang"
        );
        assert!(
            server.is_bad(),
            "a drain timeout must mark the backend bad so it is evicted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_deferred_eviction_closes_times_out_on_silent_backend() {
        // the internal Close+Sync
        // round-trip must be bounded by its deadline and mark the backend bad
        // when the backend stalls after receiving Close+Sync. Drive the inner
        // with a short deadline against a silent peer; it must return on its
        // own (outer 5s guard catches a hang) with Err and a bad backend.
        use super::Server;
        use std::time::Duration;

        let (mut server, _peer) = Server::test_silent_socket();
        server
            .deferred_eviction_closes
            .insert("DOORMAN_1".to_string());

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            server.send_deferred_eviction_closes_with_timeout(Duration::from_millis(50)),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "send_deferred_eviction_closes must return on its own deadline, not hang"
        );
        let res = outcome.expect("did not hang");
        assert!(res.is_err(), "a stalled Close round-trip must error");
        assert!(
            server.is_bad(),
            "a Close-round-trip timeout must mark the backend bad so it is evicted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn queued_deferred_close_eviction_does_not_leave_lru_stale_after_drain() {
        use super::Server;

        let mut server = Server::test_dead_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(4).unwrap(),
            ahash::RandomState::new(),
        ));
        server.add_prepared_statement_to_cache("DOORMAN_1");
        assert!(server.has_prepared_statement("DOORMAN_1"));

        server.queue_deferred_eviction_close("DOORMAN_1".to_string());
        assert!(
            server.has_prepared_statement("DOORMAN_1"),
            "pending deferred Close must still report present until PostgreSQL sees it"
        );

        server.deferred_eviction_closes.clear();
        assert!(
            !server.has_prepared_statement("DOORMAN_1"),
            "after deferred Close drains, local LRU must not claim the closed server name exists"
        );
    }

    #[tokio::test]
    async fn graceful_terminate_finisher_times_out_stalled_stream() {
        use bytes::{BufMut, BytesMut};
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tokio::io::AsyncWrite;

        struct PendingWriter;

        impl AsyncWrite for PendingWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Pending
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }

        let mut bytes = BytesMut::with_capacity(5);
        bytes.put_u8(b'X');
        bytes.put_i32(4);

        let completed =
            super::finish_graceful_terminate(PendingWriter, bytes, 0, Duration::from_millis(10))
                .await;

        assert!(
            !completed,
            "stalled graceful Terminate finisher must return after its timeout"
        );
    }

    #[test]
    fn terminate_task_guard_decrements_counter_on_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        let counter = AtomicUsize::new(0);
        let notify = Notify::new();
        {
            let _guard = super::TerminateTaskGuard::new(&counter, &notify);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "counter must be decremented even if the terminate future exits early"
        );
    }

    // Pure SyncPlan classifier coverage. These exercise the factored-out
    // `classify_sync_plan` so we do not need to build a full `Server`.
    mod compute_sync_plan_tests {
        use super::super::{classify_sync_plan, SyncPlan};
        use crate::server::parameters::ServerParameters;
        use std::collections::HashSet;

        /// Build an empty backend snapshot, then set each param so the diff
        /// stays focused on exactly what each test wants.
        fn params_with(pairs: &[(&str, &str)]) -> ServerParameters {
            let mut p = ServerParameters::new();
            for (key, value) in pairs {
                p.set_param((*key).to_string(), (*value).to_string(), true);
            }
            p
        }

        #[test]
        fn compute_sync_plan_app_name_only() {
            // Backend app_name=svc-old, client wants svc-new, nothing else.
            let backend = params_with(&[("application_name", "svc-old")]);
            let incoming = params_with(&[("application_name", "svc-new")]);
            let plan = classify_sync_plan(&backend, &HashSet::new(), &incoming)
                .expect("classify must succeed for a benign value");
            match plan {
                SyncPlan::AppNameOnly(sql) => {
                    // Byte-identical to what sync_parameters emits for this
                    // single SET, minus the trailing ';'. sql_string_literal
                    // dollar-quotes a simple value as
                    // $pgdoorman0$..$pgdoorman0$.
                    assert_eq!(
                        sql,
                        "SET application_name TO $pgdoorman0$svc-new$pgdoorman0$"
                    );
                }
                other => panic!("expected AppNameOnly, got {other:?}"),
            }
        }

        #[test]
        fn compute_sync_plan_app_name_only_from_unset_backend() {
            // Backend has no application_name at all; client sets one.
            // Still a single-key application_name SetTo -> AppNameOnly.
            let backend = ServerParameters::new();
            let incoming = params_with(&[("application_name", "svc-new")]);
            let plan = classify_sync_plan(&backend, &HashSet::new(), &incoming)
                .expect("classify must succeed");
            match plan {
                SyncPlan::AppNameOnly(sql) => assert_eq!(
                    sql,
                    "SET application_name TO $pgdoorman0$svc-new$pgdoorman0$"
                ),
                other => panic!("expected AppNameOnly, got {other:?}"),
            }
        }

        #[test]
        fn compute_sync_plan_empty_identical_params() {
            // Identical params on both sides -> empty diff -> Empty.
            let backend = params_with(&[("application_name", "svc-x"), ("search_path", "public")]);
            let incoming = params_with(&[("application_name", "svc-x"), ("search_path", "public")]);
            let plan = classify_sync_plan(&backend, &HashSet::new(), &incoming)
                .expect("classify must succeed");
            assert!(
                matches!(plan, SyncPlan::Empty),
                "expected Empty, got {plan:?}"
            );
        }

        #[test]
        fn compute_sync_plan_complex_app_name_plus_second_key() {
            // app_name change AND a second key (search_path) -> Complex.
            let backend =
                params_with(&[("application_name", "svc-old"), ("search_path", "public")]);
            let incoming = params_with(&[
                ("application_name", "svc-new"),
                ("search_path", "app, public"),
            ]);
            let plan = classify_sync_plan(&backend, &HashSet::new(), &incoming)
                .expect("classify must succeed");
            assert!(
                matches!(plan, SyncPlan::Complex),
                "expected Complex, got {plan:?}"
            );
        }

        #[test]
        fn compute_sync_plan_complex_single_non_app_name_change() {
            // A single non-app_name change (search_path only) -> Complex.
            let backend = params_with(&[("search_path", "public")]);
            let incoming = params_with(&[("search_path", "app, public")]);
            let plan = classify_sync_plan(&backend, &HashSet::new(), &incoming)
                .expect("classify must succeed");
            assert!(
                matches!(plan, SyncPlan::Complex),
                "expected Complex, got {plan:?}"
            );
        }

        #[test]
        fn compute_sync_plan_app_name_reset_is_complex() {
            // Backend has application_name set; client has none -> compare_params
            // yields {"application_name": Reset}. A single-key Reset must NOT be
            // mistaken for AppNameOnly (which requires SetTo) -> Complex. Pins the
            // exact edge a loosened guard (contains_key instead of SetTo) would regress.
            let backend = params_with(&[("application_name", "svc-old")]);
            let incoming = ServerParameters::new();
            let plan = classify_sync_plan(&backend, &HashSet::new(), &incoming)
                .expect("classify must succeed");
            assert!(
                matches!(plan, SyncPlan::Complex),
                "expected Complex for a Reset of application_name, got {plan:?}"
            );
        }

        #[test]
        fn compute_sync_plan_operator_managed_app_name_is_retained_out() {
            // If application_name is operator-managed, it is removed from the
            // diff before classification - leaving an empty diff here -> Empty.
            let backend = params_with(&[("application_name", "svc-old")]);
            let incoming = params_with(&[("application_name", "svc-new")]);
            let mut managed = HashSet::new();
            managed.insert("application_name".to_string());
            let plan =
                classify_sync_plan(&backend, &managed, &incoming).expect("classify must succeed");
            assert!(
                matches!(plan, SyncPlan::Empty),
                "expected Empty (app_name operator-managed), got {plan:?}"
            );
        }
    }
}
