//! Server connection pool manager.
//!
//! `ServerPool` manages the creation and recycling of individual PostgreSQL
//! server connections. It handles connect timeouts, lifetime checks, alive
//! checks, pause/resume, and reconnect epoch management.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::{Notify, Semaphore};

use crate::config::startup_parameters as sp;
use crate::config::{Address, User};
use crate::errors::Error;
use crate::patroni::types::Role;
use crate::server::Server;
use crate::stats::{AddressStats, ServerStats};
use crate::utils::format_duration_ms;

use super::errors::{RecycleError, RecycleResult};
use super::startup_resolver::ApplicationState;
use super::types::Metrics;
use super::ClientServerMap;

/// Decision returned by `ServerPool::classify_startup_parameters`. Used
/// by the spawn path to drive counter/log side effects and by the
/// read-only admin/API view to label each entry without touching the
/// metric. Carries the packet/body byte counts so the caller can log
/// them without recomputing.
#[derive(Debug, Clone, Copy)]
enum BudgetDecision {
    FullCascade,
    OverlayDroppedBaselineKept {
        body_bytes: usize,
        packet_bytes: usize,
    },
    EmptyDueToBudget {
        reason: BudgetReason,
        body_bytes: usize,
        packet_bytes: usize,
    },
}

#[derive(Debug, Clone, Copy)]
enum BudgetReason {
    CascadeBudgetExceeded,
    PacketCapExceeded,
}

struct PrewarmFailureGuard {
    stats: Arc<AddressStats>,
    username: String,
    pool_name: String,
    server_user: String,
    active: bool,
}

impl PrewarmFailureGuard {
    fn new(pool: &ServerPool) -> Self {
        Self {
            stats: pool.address.stats.clone(),
            username: pool.address.username.clone(),
            pool_name: pool.address.pool_name.clone(),
            server_user: pool.prewarm_username().to_string(),
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for PrewarmFailureGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.stats.prewarm_failure();
        warn!(
            "[{}@{}] prewarm_query cancelled before completion on new backend (server_user={}); \
             create timeout or task cancellation dropped the backend",
            self.username, self.pool_name, self.server_user,
        );
    }
}

impl BudgetReason {
    fn as_str(self) -> &'static str {
        match self {
            BudgetReason::CascadeBudgetExceeded => "cascade_budget_exceeded",
            BudgetReason::PacketCapExceeded => "packet_cap_exceeded",
        }
    }
}

/// Precompute the wire-ready `startup_parameters` map for a pool and
/// the budget classification reported on each backend start. Called
/// once from `ServerPool::new`; saves the startup path from
/// cloning the BTreeMap, re-running `packet_and_body_bytes`, and
/// re-evaluating the overlay-drop branch on every checkout.
///
/// Cases follow `classify_startup_parameters`:
/// 1. Empty overlay + budget OK → reuse the base `Arc`.
/// 2. Non-empty overlay + budget OK → allocate the merged map once.
/// 3. Merged over budget but baseline fits → reuse the base `Arc` and
///    log the drop once. The backend start path rejects this decision.
/// 4. Nothing fits → empty map, log the drop.
fn build_resolved_startup(
    base: &Arc<BTreeMap<String, String>>,
    overlay: &Arc<BTreeMap<String, String>>,
    server_username: &str,
    database: &str,
    application_name: &str,
    pool_name: &str,
    client_username: &str,
) -> (Arc<BTreeMap<String, String>>, BudgetDecision) {
    // PostgreSQL GUC names are case-insensitive, but `general` and
    // `pool` are deserialised as opaque maps with exact-case keys. A
    // pool that sets `TimeZone = "UTC"` over a
    // `general.timezone = "Europe/Berlin"` would otherwise ship both
    // rows in `StartupMessage` and let backend-side merge order decide
    // which wins. Canonicalise every key here so the merged view, the
    // baseline-only fallback, and the admin/API read model all agree
    // on the GUC name PostgreSQL will use.
    let canonical_base: BTreeMap<String, String> = base
        .iter()
        .map(|(k, v)| {
            (
                crate::server::parameters::canonicalize_param_name(k.clone()),
                v.clone(),
            )
        })
        .collect();
    let merged: BTreeMap<String, String> = if overlay.is_empty() {
        canonical_base.clone()
    } else {
        let mut m = canonical_base.clone();
        for (k, v) in overlay.iter() {
            m.insert(
                crate::server::parameters::canonicalize_param_name(k.clone()),
                v.clone(),
            );
        }
        m
    };
    let (packet_bytes, body_bytes) =
        sp::packet_and_body_bytes(server_username, database, application_name, &merged);
    let over_budget = body_bytes > sp::MAX_OPERATOR_BUDGET;
    let over_packet = packet_bytes > sp::MAX_STARTUP_PACKET_SIZE;
    if !over_budget && !over_packet {
        return (Arc::new(merged), BudgetDecision::FullCascade);
    }

    // Merged cascade is over a limit. Try the canonical baseline-only.
    if !overlay.is_empty() {
        let (baseline_packet, baseline_body) =
            sp::packet_and_body_bytes(server_username, database, application_name, &canonical_base);
        if baseline_body <= sp::MAX_OPERATOR_BUDGET
            && baseline_packet <= sp::MAX_STARTUP_PACKET_SIZE
        {
            warn!(
                "[{client_username}@{pool_name}] auth_query per-user startup_parameters pushes the cascade \
                 over the operator budget (merged {body_bytes} bytes, packet {packet_bytes} bytes); \
                 backend spawns will be rejected until the overlay or baseline shrinks"
            );
            return (
                Arc::new(canonical_base),
                BudgetDecision::OverlayDroppedBaselineKept {
                    body_bytes,
                    packet_bytes,
                },
            );
        }
    }

    let reason = if over_packet {
        BudgetReason::PacketCapExceeded
    } else {
        BudgetReason::CascadeBudgetExceeded
    };
    warn!(
        "[{client_username}@{pool_name}] effective startup_parameters serialize to {body_bytes} bytes \
         (packet {packet_bytes} bytes), exceeding operator budget {} / PG cap {}; \
         backend spawns will be rejected until general/pool startup_parameters shrink",
        sp::MAX_OPERATOR_BUDGET, sp::MAX_STARTUP_PACKET_SIZE,
    );
    (
        Arc::new(BTreeMap::new()),
        BudgetDecision::EmptyDueToBudget {
            reason,
            body_bytes,
            packet_bytes,
        },
    )
}

/// Wrapper for the connection pool.
pub struct ServerPool {
    /// Server address.
    address: Address,

    /// Pool user.
    user: User,

    /// Server database.
    database: String,

    /// Client/server mapping.
    client_server_map: ClientServerMap,

    /// Should we clean up dirty connections before putting them into the pool?
    cleanup_connections: bool,

    application_name: String,

    /// Log client parameter status changes
    log_client_parameter_status_changes: bool,

    /// Prepared statement cache size
    prepared_statement_cache_size: usize,

    /// Semaphore to limit concurrent server connection creation.
    create_semaphore: Arc<Semaphore>,

    /// Counter for total connections created (for logging).
    connection_counter: AtomicU64,

    /// Server lifetime in milliseconds (0 = unlimited).
    lifetime_ms: u64,

    /// Idle timeout in milliseconds (0 = disabled).
    /// Connections idle longer than this are closed by retain.
    idle_timeout_ms: u64,

    /// Time after which idle connections should be checked before reuse (0 = disabled).
    idle_check_timeout_ms: u64,

    /// Connect timeout for alive checks and main-path startup deadline.
    connect_timeout: Duration,

    /// Hard upper bound on how long a single client may wait for a server
    /// connection. Used as the outer deadline around the entire fallback
    /// path: there's no point spending more time than the client itself is
    /// willing to wait. Sourced from `general.query_wait_timeout`.
    query_wait_timeout: Duration,

    /// Session mode flag passed to created Server connections.
    session_mode: bool,

    /// Patroni-assisted fallback state.
    fallback_state: Option<Arc<super::fallback::FallbackState>>,

    /// Combined pool state: bit 32 = paused, bits 0-31 = reconnect epoch (u32).
    pool_state: AtomicU64,

    /// Notify to wake up clients blocked on PAUSE.
    resume_notify: Notify,

    /// Per-user auth_query overlay captured at pool construction. Dynamic
    /// passthrough pools populate this from a fresh `cache.get_or_fetch`
    /// snapshot taken right after auth, so every backend spawn from this
    /// pool sees the same overlay even when the auth_query cache TTL has
    /// since expired. Empty for static pools and for the dedicated-mode
    /// shared pool (which intentionally has no per-user override).
    /// Refetches that change the overlay are handled by the reload /
    /// drain logic in `pool/mod.rs`; this field is immutable for the
    /// lifetime of the pool object.
    per_user_startup_overlay: Arc<std::collections::BTreeMap<String, String>>,

    /// Canonicalized GUC names this pool injects through
    /// `startup_parameters` (general + pool + auth_query overlay). The
    /// backend startup path (`Server::startup`) uses this set so that
    /// `sync_parameters` on checkout does not let a client value overwrite
    /// the operator default. The client startup path filters the
    /// `StartupMessage` parameters against the same set before sending
    /// `ParameterStatus`, so a driver sees the same value PG will use for
    /// the session. Built once per pool and shared as `Arc`.
    operator_managed_startup_keys: Arc<HashSet<String>>,

    /// Wire-ready merged startup map, precomputed at pool creation. The
    /// spawn path used to recompute this on every backend create — clone
    /// the baseline, merge the auth_query overlay, recheck the budget.
    /// `base_startup_parameters` and `per_user_startup_overlay` are
    /// immutable for the pool's lifetime, so the merge is too: every call
    /// can hand out an `Arc` to the same map instead of cloning a
    /// 50-entry BTreeMap and re-running `packet_and_body_bytes`.
    resolved_startup_map: Arc<BTreeMap<String, String>>,

    /// Budget classification for `resolved_startup_map`. Precomputed
    /// together with the map so the spawn path skips the re-validation
    /// walk on every backend create.
    resolved_startup_decision: BudgetDecision,

    /// Pool-level `release_query` config snapshot. `None` => use the
    /// iServ-compatible default (`RELEASE_SESSION_QUERY`); `Some("")` => disable the
    /// release query entirely; `Some(custom)` => execute exactly that SQL.
    /// Installed via `ServerPool::with_release_query` from `mod.rs` /
    /// `dynamic.rs`; consumed in `create()` and `create_fallback_connection_inner()`
    /// to call `Server::set_release_query` right after startup.
    release_query: Option<String>,

    /// Effective `prewarm_query` for this pool - already resolved against the
    /// per-user override at construction time. Empty string disables the
    /// prewarm. Executed once after `Server::startup` in `create()` and
    /// `create_fallback_connection_inner()`; a SQL or transport failure marks
    /// the backend bad so the connection never reaches the idle queue.
    prewarm_query: String,

    /// Pool-level switch for the DISCARD ALL synthetic-response fast path
    /// (see `Pool.intercept_discard_all` for the operator-facing semantics).
    /// Propagated to each `Server` via `set_intercept_discard_all` right
    /// after startup so the per-backend gate in `handle_simple_query` does
    /// not need to look the config up on every query.
    intercept_discard_all: bool,
}

impl std::fmt::Debug for ServerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPool")
            .field("address", &self.address)
            .field("user", &self.user)
            .field("database", &self.database)
            .field("cleanup_connections", &self.cleanup_connections)
            .field("application_name", &self.application_name)
            .field(
                "log_client_parameter_status_changes",
                &self.log_client_parameter_status_changes,
            )
            .field(
                "prepared_statement_cache_size",
                &self.prepared_statement_cache_size,
            )
            .field(
                "connection_counter",
                &self.connection_counter.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl ServerPool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Address,
        user: User,
        database: &str,
        client_server_map: ClientServerMap,
        cleanup_connections: bool,
        log_client_parameter_status_changes: bool,
        prepared_statement_cache_size: usize,
        application_name: String,
        max_concurrent_creates: usize,
        lifetime_ms: u64,
        idle_timeout_ms: u64,
        idle_check_timeout_ms: u64,
        connect_timeout: Duration,
        query_wait_timeout: Duration,
        session_mode: bool,
        fallback_state: Option<Arc<super::fallback::FallbackState>>,
        base_startup_parameters: Arc<BTreeMap<String, String>>,
        per_user_startup_overlay: Arc<BTreeMap<String, String>>,
    ) -> ServerPool {
        let operator_managed_startup_keys = Arc::new(
            base_startup_parameters
                .keys()
                .chain(per_user_startup_overlay.keys())
                .map(|k| crate::server::parameters::canonicalize_param_name(k.clone()))
                .collect::<HashSet<String>>(),
        );
        let server_username = user
            .server_username
            .as_deref()
            .unwrap_or(user.username.as_str());
        let (resolved_startup_map, resolved_startup_decision) = build_resolved_startup(
            &base_startup_parameters,
            &per_user_startup_overlay,
            server_username,
            database,
            &application_name,
            address.pool_name.as_str(),
            user.username.as_str(),
        );
        ServerPool {
            address,
            user: user.clone(),
            database: database.to_string(),
            client_server_map,
            cleanup_connections,
            log_client_parameter_status_changes,
            prepared_statement_cache_size,
            create_semaphore: Arc::new(Semaphore::new(max_concurrent_creates)),
            connection_counter: AtomicU64::new(0),
            application_name,
            lifetime_ms,
            idle_timeout_ms,
            idle_check_timeout_ms,
            connect_timeout,
            query_wait_timeout,
            pool_state: AtomicU64::new(0),
            resume_notify: Notify::new(),
            session_mode,
            fallback_state,
            per_user_startup_overlay,
            operator_managed_startup_keys,
            resolved_startup_map,
            resolved_startup_decision,
            release_query: None,
            prewarm_query: String::new(),
            // Default-safe: honour the iServ contract even when nobody
            // calls `with_intercept_discard_all` (test helpers, GC paths).
            intercept_discard_all: true,
        }
    }

    /// Builder-style override for the resolved release-query of this pool.
    /// Called by the static (`pool/mod.rs`) and dynamic (`pool/dynamic.rs`)
    /// pool init paths to forward `pool_config.release_query`. The default
    /// (constructed in `ServerPool::new`) is `None`, which means
    /// `resolve_release_query` will materialise the iServ-compatible default
    /// when the backend is created.
    pub fn with_release_query(mut self, release_query: Option<String>) -> ServerPool {
        self.release_query = release_query;
        self
    }

    /// Builder-style override for the effective prewarm SQL. Callers must
    /// resolve the per-user override against `pool_config.prewarm_query`
    /// before invoking this (`user.prewarm_query` wins entirely when `Some`,
    /// even `Some(String::new())` which disables the prewarm for that user).
    /// Empty string here means "no prewarm".
    pub fn with_prewarm_query(mut self, prewarm_query: String) -> ServerPool {
        self.prewarm_query = prewarm_query;
        self
    }

    /// Builder-style override for the DISCARD ALL interception fast path.
    /// Mirrors `Pool.intercept_discard_all`: `true` (default) makes
    /// pg_doorman synthesise the response locally without forwarding;
    /// `false` lets the query reach PostgreSQL as a normal simple query.
    pub fn with_intercept_discard_all(mut self, intercept: bool) -> ServerPool {
        self.intercept_discard_all = intercept;
        self
    }

    /// Effective backend username for prewarm error reporting. Mirrors the
    /// rule used by `Server::startup` so the log line points at the actual
    /// PostgreSQL role rather than the pool's frontend role.
    fn prewarm_username(&self) -> &str {
        self.user
            .server_username
            .as_deref()
            .unwrap_or(self.user.username.as_str())
    }

    /// Execute the configured prewarm SQL once on a freshly authenticated
    /// backend. The wrapper around `small_simple_query` here keeps the
    /// call-site in `create()` short and ensures failures (transport `Err`,
    /// SQL `ErrorResponse`, or cancellation by the caller's create timeout)
    /// emit a warn log and bump the per-address counter. Explicit query
    /// errors also mark the backend bad before the `?` exit at the call-site
    /// drops the connection.
    ///
    /// **Timeout note**: `small_simple_query` enforces the housekeeping
    /// deadline (30s, see `HOUSEKEEPING_TIMEOUT` in `server_backend.rs`).
    /// The same cap applies here - a slow prewarm statement (`VACUUM
    /// ANALYZE`, `pg_prewarm`, large-table cache hydration) that does not
    /// complete within 30s will be killed and the backend marked bad,
    /// causing every new connection to churn. Keep `prewarm_query` to
    /// inexpensive setup SQL (planner GUCs, lightweight `SELECT 1`, plan
    /// warming via `EXECUTE`), not to bulk data preload. The 4096-byte
    /// config validation cap (`config/pool.rs`) is an additional defence
    /// against pasting an entire migration script in here.
    async fn run_prewarm_query(&self, conn: &mut crate::server::Server) -> Result<(), Error> {
        if self.prewarm_query.trim().is_empty() {
            return Ok(());
        }
        let mut failure_guard = PrewarmFailureGuard::new(self);
        match conn.small_simple_query(&self.prewarm_query).await {
            Ok(()) => {
                if let Some(violation) = conn.recycle_safety_violation() {
                    failure_guard.disarm();
                    self.address.stats.prewarm_failure();
                    let reason = format!("prewarm_query left backend non-recyclable: {violation}");
                    warn!(
                        "[{}@{}] prewarm_query left new backend unsafe for pooling \
                         (server_user={}): {violation}",
                        self.address.username,
                        self.address.pool_name,
                        self.prewarm_username(),
                    );
                    conn.mark_bad(&reason);
                    return Err(Error::ProtocolSyncError(reason));
                }
                failure_guard.disarm();
                Ok(())
            }
            Err(err) => {
                failure_guard.disarm();
                self.address.stats.prewarm_failure();
                warn!(
                    "[{}@{}] prewarm_query failed on new backend (server_user={}): {err}",
                    self.address.username,
                    self.address.pool_name,
                    self.prewarm_username(),
                );
                conn.mark_bad("prewarm_query failed");
                Err(err)
            }
        }
    }

    /// See `operator_managed_startup_keys` field.
    pub fn operator_managed_startup_keys(&self) -> Arc<HashSet<String>> {
        self.operator_managed_startup_keys.clone()
    }

    /// Attempts to create a new connection.
    /// Uses a semaphore to limit concurrent connection creation instead of serializing with mutex.
    pub async fn create(&self) -> Result<Server, Error> {
        // Acquire semaphore permit to limit concurrent creates
        let _permit = self
            .create_semaphore
            .acquire()
            .await
            .map_err(|_| Error::ServerStartupReadParameters("Semaphore closed".to_string()))?;

        // Local backend is in cooldown — skip directly to fallback.
        // JustExpired bumps epoch to drain stale fallback connections.
        if let Some(ref fallback) = self.fallback_state {
            use super::fallback::BlacklistCheck;
            match fallback.check_blacklist() {
                BlacklistCheck::Active => {
                    if fallback.should_log_blacklist() {
                        info!(
                            "[{}@{}] fallback: local backend in cooldown, routing to fallback",
                            self.address.username, self.address.pool_name,
                        );
                    } else {
                        debug!(
                            "[{}@{}] fallback: local backend in cooldown, routing to fallback",
                            self.address.username, self.address.pool_name,
                        );
                    }
                    match self.create_fallback_connection().await {
                        Ok(conn) => return Ok(conn),
                        Err(err) => {
                            warn!(
                                "[{}@{}] fallback: connection failed during cooldown: {err}",
                                self.address.username, self.address.pool_name,
                            );
                            // Fall through to try the local backend anyway
                        }
                    }
                }
                BlacklistCheck::JustExpired => {
                    info!(
                        "[{}@{}] fallback: cooldown expired, resuming local backend",
                        self.address.username, self.address.pool_name,
                    );
                    self.bump_epoch();
                }
                BlacklistCheck::NotBlacklisted => {}
            }
        }

        let conn_num = self.connection_counter.fetch_add(1, Ordering::Relaxed) + 1;
        info!(
            "[{}@{}] new server connection #{} to {}:{}",
            self.address.username,
            self.address.pool_name,
            conn_num,
            self.address.host,
            self.address.port,
        );
        // Parameter-budget rejection happens before a server socket exists.
        // Leave SERVER_STATS untouched so SHOW SERVERS never lists a backend
        // startup that did not open a socket. The plain attempt and
        // sslmode=allow retry share this resolved map.
        let startup_parameters = self.resolved_startup_parameters()?;

        let (stats, registration) =
            ServerStats::new_registered(self.address.clone(), crate::utils::clock::now());

        let result = startup_with_timeout(
            self.connect_timeout,
            &self.address.host,
            self.address.port,
            Server::startup(
                &self.address,
                &self.user,
                &self.database,
                self.client_server_map.clone(),
                stats.clone(),
                self.cleanup_connections,
                self.log_client_parameter_status_changes,
                self.prepared_statement_cache_size,
                self.application_name.clone(),
                self.session_mode,
                &startup_parameters,
                self.operator_managed_startup_keys.clone(),
            ),
        )
        .await;

        // libpq sslmode=allow: PostgreSQL has no protocol-level "TLS required"
        // signal. pg_hba rejects plain connections with FATAL 28000 after the
        // StartupMessage, and the socket cannot be reused after that. Match
        // libpq by retrying startup failures over TLS, but skip transport
        // failures where no PostgreSQL startup response was received.
        //
        // Reference: PostgreSQL docs, "SSL Support" → sslmode parameter.
        let should_tls_retry = match &result {
            Err(err) if self.address.server_tls.mode.retries_with_tls() => !matches!(
                err,
                Error::ConnectError(_)
                    | Error::ConnectResourceExhausted(_)
                    | Error::ServerUnavailableError(_, _)
                    | Error::ServerStartupParameterRejection { .. }
            ),
            _ => false,
        };
        let (result, active_registration) = if should_tls_retry {
            info!(
                "plain connection rejected, retrying with tls, user={} pool={} host={} port={} server_tls_mode=allow",
                self.address.username, self.address.pool_name,
                self.address.host, self.address.port,
            );
            // The plain attempt ended; keep only the TLS retry row visible.
            drop(registration);
            drop(stats);
            let mut retry_address = self.address.clone();
            retry_address.server_tls = std::sync::Arc::new(crate::config::tls::ServerTlsConfig {
                mode: crate::config::tls::ServerTlsMode::Require,
                connector: self.address.server_tls.connector.clone(),
                cert_hash: self.address.server_tls.cert_hash,
            });
            let (retry_stats, retry_registration) =
                ServerStats::new_registered(self.address.clone(), crate::utils::clock::now());
            let retry_result = startup_with_timeout(
                self.connect_timeout,
                &retry_address.host,
                retry_address.port,
                Server::startup(
                    &retry_address,
                    &self.user,
                    &self.database,
                    self.client_server_map.clone(),
                    retry_stats.clone(),
                    self.cleanup_connections,
                    self.log_client_parameter_status_changes,
                    self.prepared_statement_cache_size,
                    self.application_name.clone(),
                    self.session_mode,
                    &startup_parameters,
                    self.operator_managed_startup_keys.clone(),
                ),
            )
            .await;
            (retry_result, retry_registration)
        } else {
            (result, registration)
        };

        match result {
            Ok(mut conn) => {
                active_registration.release();
                // Forward the pool-level release_query onto this backend before
                // it joins the idle set. Empty / None / custom are all decided
                // by `resolve_release_query`.
                conn.set_release_query(self.release_query.as_deref());
                conn.set_intercept_discard_all(self.intercept_discard_all);
                // Run prewarm_query (if configured). On failure the backend is
                // marked bad inside the helper; we surface the error so the
                // pool drops this connection (Server::Drop closes the socket).
                self.run_prewarm_query(&mut conn).await?;
                // Permit is released automatically when _permit goes out of scope
                conn.stats.idle(0);
                Ok(conn)
            }
            Err(err) => {
                // The local attempt ended before fallback or backoff starts.
                // Remove its login row before the next attempt publishes one.
                drop(active_registration);
                // Local backend unreachable + Patroni-assisted fallback configured: route via fallback.
                if is_backend_unreachable(&err) {
                    if let Some(ref fallback) = self.fallback_state {
                        fallback.blacklist();
                        crate::web::metrics::record_pool_only_fallback_tls_label(
                            &self.address.pool_name,
                        );
                        crate::web::metrics::FALLBACK_ACTIVE
                            .with_label_values(&[&self.address.pool_name])
                            .set(1.0);
                        info!(
                            "[{}@{}] fallback: routing through fallback (original error: {err})",
                            self.address.username, self.address.pool_name,
                        );
                        return self.create_fallback_connection().await;
                    }
                }
                // Brief backoff on error to avoid hammering a failing server
                tokio::time::sleep(Duration::from_millis(10)).await;
                Err(err)
            }
        }
    }

    /// Returns the address of this pool.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Return the effective startup parameter cascade with the winning
    /// source layer **and** the application state for each key. Used by
    /// `SHOW STARTUP_PARAMETERS` and `/api/pools`.
    ///
    /// The cascade view comes from the live config and the auth_query
    /// cache, so it reflects what the operator just edited. The
    /// application state cross-checks each key against
    /// `resolved_startup_parameters()` — the same wire-ready map that
    /// `Server::startup` will ship for the next backend spawn — so the
    /// admin view can flag keys that look configured but will not
    /// actually leave the wire:
    ///
    /// * `Applied` — same key/value in both views; the next spawn
    ///   ships it.
    /// * `DroppedDueToBudget` — the wire map omits the key. The runtime
    ///   budget/packet check dropped the operator cascade (or the
    ///   overlay) on the most recent spawn; same will happen on the
    ///   next one until the operator shrinks the config.
    /// * `Stale` — the key is in the wire map but with a different
    ///   value, which means the frozen `base_startup_parameters` /
    ///   `per_user_startup_overlay` Arc on this pool was captured
    ///   before the operator's latest edit. RELOAD has not yet
    ///   recycled the pool (general/pool change) or the auth_query
    ///   cache has not refetched (per-user change). The next spawn
    ///   ships the stale value, not the configured one.
    pub fn effective_startup_parameters_with_sources(
        &self,
    ) -> std::collections::BTreeMap<
        String,
        (
            String,
            super::startup_resolver::ParameterSource,
            ApplicationState,
        ),
    > {
        let cfg = crate::config::config_arc();
        let pool_params = cfg
            .pools
            .get(&self.address.pool_name)
            .map(|p| &p.startup_parameters)
            .cloned()
            .unwrap_or_default();
        let auth_query_params: Option<std::collections::HashMap<String, String>> =
            match super::get_auth_query_state(&self.address.pool_name) {
                Some(state) if !state.config.is_dedicated_mode() => {
                    state.peek_startup_parameters(&self.user.username, |m| m.clone())
                }
                _ => None,
            };
        let configured = super::startup_resolver::resolve_with_sources(
            &cfg.general.startup_parameters,
            &pool_params,
            auth_query_params.as_ref(),
        );
        // Use the pure classifier so admin/API polling has no metric or log
        // side effects.
        let (wire_cow, decision) = self.classify_startup_parameters();
        let wire = wire_cow.as_ref();
        // If the cascade was rejected for budget, report every configured
        // key as dropped.
        let cascade_rejected = matches!(
            decision,
            BudgetDecision::OverlayDroppedBaselineKept { .. }
                | BudgetDecision::EmptyDueToBudget { .. }
        );
        let mut out: std::collections::BTreeMap<
            String,
            (
                String,
                super::startup_resolver::ParameterSource,
                ApplicationState,
            ),
        > = configured
            .into_iter()
            .map(|(k, (v, src))| {
                let state = if cascade_rejected {
                    ApplicationState::DroppedDueToBudget
                } else {
                    match wire.get(&k) {
                        Some(wire_v) if wire_v == &v => ApplicationState::Applied,
                        Some(_) => ApplicationState::Stale,
                        None => ApplicationState::DroppedDueToBudget,
                    }
                };
                (k, (v, src, state))
            })
            .collect();
        // Surface wire-only stale keys from the frozen baseline or auth_query
        // overlay.
        let wire_state = if cascade_rejected {
            ApplicationState::DroppedDueToBudget
        } else {
            ApplicationState::Stale
        };
        for (k, wire_v) in wire {
            if !out.contains_key(k) {
                let frozen_source = if self.per_user_startup_overlay.contains_key(k) {
                    super::startup_resolver::ParameterSource::AuthQuery
                } else {
                    super::startup_resolver::ParameterSource::Pool
                };
                out.insert(k.clone(), (wire_v.clone(), frozen_source, wire_state));
            }
        }
        out
    }

    /// Read-only startup-parameter classifier shared by spawn and admin/API.
    fn classify_startup_parameters(
        &self,
    ) -> (
        std::borrow::Cow<'_, BTreeMap<String, String>>,
        BudgetDecision,
    ) {
        (
            std::borrow::Cow::Borrowed(&*self.resolved_startup_map),
            self.resolved_startup_decision,
        )
    }

    /// Startup parameters for one backend spawn.
    ///
    /// Overflow becomes `ServerStartupParameterRejection` instead of
    /// silently dropping configured parameters.
    fn resolved_startup_parameters(
        &self,
    ) -> Result<std::borrow::Cow<'_, BTreeMap<String, String>>, Error> {
        let (map, decision) = self.classify_startup_parameters();
        match decision {
            BudgetDecision::FullCascade => Ok(map),
            BudgetDecision::OverlayDroppedBaselineKept {
                body_bytes,
                packet_bytes,
            } => {
                crate::web::metrics::observe_startup_parameters_dropped(
                    self.address.pool_name.as_str(),
                    "auth_query_overlay_oversize",
                );
                Err(Error::ServerStartupParameterRejection {
                    sqlstate: "53400".to_string(),
                    message: format!(
                        "auth_query startup_parameters for pool '{}' would exceed the \
                         operator budget (merged body {} bytes, full packet {} bytes; \
                         operator budget {} bytes, PG StartupMessage cap {} bytes). \
                         Reduce the per-user startup_parameters row, the pool baseline, \
                         or the general baseline.",
                        self.address.pool_name,
                        body_bytes,
                        packet_bytes,
                        sp::MAX_OPERATOR_BUDGET,
                        sp::MAX_STARTUP_PACKET_SIZE,
                    ),
                    server_identifier: crate::app::errors::ServerIdentifier::new(
                        self.user.username.clone(),
                        &self.database,
                        &self.address.pool_name,
                    ),
                })
            }
            BudgetDecision::EmptyDueToBudget {
                reason,
                body_bytes,
                packet_bytes,
            } => {
                crate::web::metrics::observe_startup_parameters_dropped(
                    self.address.pool_name.as_str(),
                    reason.as_str(),
                );
                Err(Error::ServerStartupParameterRejection {
                    sqlstate: "53400".to_string(),
                    message: format!(
                        "startup_parameters cascade for pool '{}' does not fit the operator \
                         budget (body {} bytes, full packet {} bytes; operator budget {} bytes, \
                         PG StartupMessage cap {} bytes). Reduce general or pool \
                         startup_parameters.",
                        self.address.pool_name,
                        body_bytes,
                        packet_bytes,
                        sp::MAX_OPERATOR_BUDGET,
                        sp::MAX_STARTUP_PACKET_SIZE,
                    ),
                    server_identifier: crate::app::errors::ServerIdentifier::new(
                        self.user.username.clone(),
                        &self.database,
                        &self.address.pool_name,
                    ),
                })
            }
        }
    }

    /// Establish a fallback connection within `query_wait_timeout`.
    async fn create_fallback_connection(&self) -> Result<Server, Error> {
        // Outer deadline bounds total fallback time for this checkout.
        let deadline = self.query_wait_timeout;
        info!(
            "[{}@{}] fallback: local backend unavailable, entering fallback path (deadline={}ms)",
            self.address.username,
            self.address.pool_name,
            deadline.as_millis()
        );
        match tokio::time::timeout(deadline, self.create_fallback_connection_inner()).await {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    "[{}@{}] fallback: outer deadline {}ms exceeded — aborting",
                    self.address.username,
                    self.address.pool_name,
                    deadline.as_millis()
                );
                Err(Error::ConnectError(format!(
                    "fallback total deadline {}ms exceeded",
                    deadline.as_millis()
                )))
            }
        }
    }

    /// Fallback body plus one stale-whitelist retry.
    async fn create_fallback_connection_inner(&self) -> Result<Server, Error> {
        let fallback = match self.fallback_state.as_ref() {
            Some(fb) => fb,
            None => {
                return Err(Error::ConnectError(
                    "fallback path entered without configured fallback_state".to_string(),
                ));
            }
        };

        let (result, source) = self.run_fallback_round(fallback).await;
        match result {
            Ok(conn) => Ok(conn),
            // Failures that no candidate switch can fix keep their typed error.
            Err(err) if is_host_independent_error(&err) => Err(err),
            Err(err) => match source {
                super::fallback::TargetSource::WhitelistCache => {
                    // Cached host may be stale; retry once with discovery.
                    info!(
                        "[{}@{}] fallback: whitelist round failed ({err}), retrying with fresh discovery",
                        self.address.username, self.address.pool_name,
                    );
                    fallback.clear_whitelist();
                    let (retry_result, _) = self.run_fallback_round(fallback).await;
                    retry_result.map_err(|e2| {
                        if is_host_independent_error(&e2) {
                            e2
                        } else {
                            Error::ConnectError(format!(
                                "fallback exhausted (whitelist round: {err}; discovery round: {e2})"
                            ))
                        }
                    })
                }
                super::fallback::TargetSource::Discovery => Err(err),
            },
        }
    }

    /// Run one fallback round.
    ///
    /// Discovery races sync_standby first, then every other candidate.
    /// Whitelist-cache hits run a single target. Host-independent typed errors
    /// are preserved.
    async fn run_fallback_round(
        &self,
        fallback: &super::fallback::FallbackState,
    ) -> (Result<Server, Error>, super::fallback::TargetSource) {
        let (targets, source) = match fallback.get_fallback_targets().await {
            Ok(pair) => pair,
            Err(e) => {
                crate::web::metrics::record_pool_only_fallback_tls_label(&self.address.pool_name);
                warn!(
                    "[{}@{}] fallback: discovery failed: {e}",
                    self.address.username, self.address.pool_name,
                );
                return (
                    Err(Error::ConnectError(format!(
                        "fallback discovery failed: {e}"
                    ))),
                    // No automatic retry after discovery failure.
                    super::fallback::TargetSource::Discovery,
                );
            }
        };

        // Startup parameters are host-independent; resolve once per round.
        let startup_parameters_round = match self.resolved_startup_parameters() {
            Ok(map) => map,
            Err(err) => return (Err(err), source),
        };

        // Whitelist-cache hit: single target, race-of-one is just a startup.
        if matches!(source, super::fallback::TargetSource::WhitelistCache) {
            let target = match targets.into_iter().next() {
                Some(t) => t,
                None => {
                    return (
                        Err(Error::ConnectError(
                            "whitelist round produced no target".into(),
                        )),
                        source,
                    )
                }
            };
            info!(
                "[{}@{}] fallback: whitelist hit, starting up {}:{} (role={:?})",
                self.address.username,
                self.address.pool_name,
                target.host,
                target.port,
                target.role
            );
            return match self
                .try_fallback_target(&target, &startup_parameters_round)
                .await
            {
                Ok(server) => {
                    record_fallback_connection_outcome(&self.address.pool_name, true);
                    fallback.set_whitelisted(target.host, target.port, target.role);
                    (Ok(server), source)
                }
                Err(err) => {
                    let reason = super::fallback::FailureReason::from(&err);
                    fallback.mark_unhealthy(&target.host, target.port, reason);
                    (Err(err), source)
                }
            };
        }

        // Discovery: partition candidates into wave 1 (sync_standby) and
        // wave 2 (everything else, in discovery order).
        let (sync_targets, other_targets): (Vec<_>, Vec<_>) = targets
            .into_iter()
            .partition(|t| matches!(t.role, Role::SyncStandby));

        let mut summary = FailureSummary::default();

        // Wave 1.
        if !sync_targets.is_empty() {
            info!(
                "[{}@{}] fallback: wave 1 — racing {} sync_standby candidate(s) ({})",
                self.address.username,
                self.address.pool_name,
                sync_targets.len(),
                format_target_list(&sync_targets),
            );
            if let Some(server) = self
                .race_wave(
                    fallback,
                    &sync_targets,
                    &mut summary,
                    source,
                    &startup_parameters_round,
                )
                .await
            {
                return (Ok(server), source);
            }
            info!(
                "[{}@{}] fallback: wave 1 exhausted ({} sync_standby), advancing to wave 2",
                self.address.username,
                self.address.pool_name,
                sync_targets.len(),
            );
        } else {
            info!(
                "[{}@{}] fallback: no sync_standby in cluster, going straight to wave 2",
                self.address.username, self.address.pool_name,
            );
        }

        // Wave 2.
        if !other_targets.is_empty() {
            info!(
                "[{}@{}] fallback: wave 2 — racing {} candidate(s) ({})",
                self.address.username,
                self.address.pool_name,
                other_targets.len(),
                format_target_list(&other_targets),
            );
            if let Some(server) = self
                .race_wave(
                    fallback,
                    &other_targets,
                    &mut summary,
                    source,
                    &startup_parameters_round,
                )
                .await
            {
                return (Ok(server), source);
            }
        }

        let summary_str = summary.format();
        warn!(
            "[{}@{}] fallback: all fallback candidates rejected ({summary_str})",
            self.address.username, self.address.pool_name,
        );
        // Surface typed startup-parameter rejection; another host cannot fix it.
        if summary.has_startup_parameter_rejection() {
            if let Some(err) = summary.startup_rejection() {
                return (Err(err), source);
            }
        }
        // Preserve local fd exhaustion as a pooler resource error.
        if summary.has_resource_exhaustion() {
            if let Some(err) = summary.resource_exhaustion() {
                return (Err(err), source);
            }
        }
        (
            Err(Error::ConnectError(format!(
                "all fallback candidates rejected ({summary_str})"
            ))),
            source,
        )
    }

    /// Race a fallback wave; whitelist winner, record loser failures.
    async fn race_wave(
        &self,
        fallback: &super::fallback::FallbackState,
        targets: &[super::fallback::FallbackTarget],
        summary: &mut FailureSummary,
        source: super::fallback::TargetSource,
        startup_parameters: &BTreeMap<String, String>,
    ) -> Option<Server> {
        let _ = source; // reserved for future wave-source-specific logic

        let futures: Vec<futures::future::BoxFuture<'_, Result<Server, Error>>> = targets
            .iter()
            .map(|t| Box::pin(self.try_fallback_target(t, startup_parameters)) as _)
            .collect();

        match race_first_success(futures).await {
            Ok((server, idx)) => {
                let winner = &targets[idx];
                record_fallback_connection_outcome(&self.address.pool_name, true);
                info!(
                    "[{}@{}] fallback: winner {}:{} (role={:?}) — startup ok",
                    self.address.username,
                    self.address.pool_name,
                    winner.host,
                    winner.port,
                    winner.role,
                );
                fallback.set_whitelisted(winner.host.clone(), winner.port, winner.role.clone());
                Some(server)
            }
            Err(errors) => {
                for (idx, err) in errors {
                    let target = &targets[idx];
                    let reason = super::fallback::FailureReason::from(&err);
                    fallback.mark_unhealthy(&target.host, target.port, reason);
                    if fallback.should_log_unhealthy(&target.host, target.port) {
                        warn!(
                            "[{}@{}] fallback: {}:{} rejected ({})",
                            self.address.username,
                            self.address.pool_name,
                            target.host,
                            target.port,
                            err
                        );
                    } else {
                        debug!(
                            "[{}@{}] fallback: {}:{} rejected ({}, suppressed)",
                            self.address.username,
                            self.address.pool_name,
                            target.host,
                            target.port,
                            err
                        );
                    }
                    summary.record(err, reason);
                }
                None
            }
        }
    }

    /// Try one fallback target with optional sslmode=allow TLS retry.
    async fn try_fallback_target(
        &self,
        target: &super::fallback::FallbackTarget,
        startup_parameters: &BTreeMap<String, String>,
    ) -> Result<Server, Error> {
        // Use the fallback per-candidate startup timeout.
        let fallback_timeout = self
            .fallback_state
            .as_ref()
            .map(|fb| fb.connect_timeout())
            .unwrap_or(self.connect_timeout);

        let mut fallback_address = self.address.clone();
        fallback_address.host = target.host.clone();
        fallback_address.port = target.port;

        let (stats, registration) =
            ServerStats::new_registered(fallback_address.clone(), crate::utils::clock::now());

        // Startup parameters are resolved once per fallback round; each
        // candidate reuses the same per-pool cascade.

        let result = startup_with_timeout(
            fallback_timeout,
            &fallback_address.host,
            fallback_address.port,
            Server::startup(
                &fallback_address,
                &self.user,
                &self.database,
                self.client_server_map.clone(),
                stats.clone(),
                self.cleanup_connections,
                self.log_client_parameter_status_changes,
                self.prepared_statement_cache_size,
                self.application_name.clone(),
                self.session_mode,
                startup_parameters,
                self.operator_managed_startup_keys.clone(),
            ),
        )
        .await;

        // Same sslmode=allow retry as the local-backend path: TLS only when the
        // server rejected us at the protocol level, never on transport failures.
        let should_tls_retry = match &result {
            Err(err) if fallback_address.server_tls.mode.retries_with_tls() => !matches!(
                err,
                Error::ConnectError(_)
                    | Error::ConnectResourceExhausted(_)
                    | Error::ServerUnavailableError(_, _)
                    | Error::ServerStartupParameterRejection { .. }
            ),
            _ => false,
        };
        let (result, active_registration) = if should_tls_retry {
            info!(
                "[{}@{}] fallback: plain connection to {}:{} rejected, retrying with tls",
                self.address.username,
                self.address.pool_name,
                fallback_address.host,
                fallback_address.port,
            );
            // The plain candidate ended; keep only the TLS retry row visible.
            drop(registration);
            drop(stats);
            let mut retry_address = fallback_address.clone();
            retry_address.server_tls = std::sync::Arc::new(crate::config::tls::ServerTlsConfig {
                mode: crate::config::tls::ServerTlsMode::Require,
                connector: fallback_address.server_tls.connector.clone(),
                cert_hash: fallback_address.server_tls.cert_hash,
            });
            let (retry_stats, retry_registration) =
                ServerStats::new_registered(fallback_address.clone(), crate::utils::clock::now());
            let retry_result = startup_with_timeout(
                fallback_timeout,
                &retry_address.host,
                retry_address.port,
                Server::startup(
                    &retry_address,
                    &self.user,
                    &self.database,
                    self.client_server_map.clone(),
                    retry_stats.clone(),
                    self.cleanup_connections,
                    self.log_client_parameter_status_changes,
                    self.prepared_statement_cache_size,
                    self.application_name.clone(),
                    self.session_mode,
                    startup_parameters,
                    self.operator_managed_startup_keys.clone(),
                ),
            )
            .await;
            (retry_result, retry_registration)
        } else {
            (result, registration)
        };

        match result {
            Ok(mut conn) => {
                active_registration.release();
                // Same release_query / prewarm_query installation as the main
                // `create()` path so fallback backends honour the operator
                // config too.
                conn.set_release_query(self.release_query.as_deref());
                conn.set_intercept_discard_all(self.intercept_discard_all);
                self.run_prewarm_query(&mut conn).await?;
                conn.stats.idle(0);
                conn.override_lifetime_ms = Some(target.lifetime_ms);
                Ok(conn)
            }
            Err(err) => {
                // No Server owns this fallback candidate, so the startup
                // registration removes its login row here.
                Err(err)
            }
        }
    }

    /// Returns the base lifetime in milliseconds for connections in this pool.
    pub fn lifetime_ms(&self) -> u64 {
        self.lifetime_ms
    }

    /// Returns the base idle timeout in milliseconds for connections in this pool.
    pub fn idle_timeout_ms(&self) -> u64 {
        self.idle_timeout_ms
    }

    /// Bit flag for the paused state within `pool_state`.
    const PAUSED_BIT: u64 = 1 << 32;
    /// Mask for the reconnect epoch (lower 32 bits) within `pool_state`.
    const EPOCH_MASK: u64 = 0xFFFF_FFFF;

    /// Returns whether the pool is paused.
    pub fn is_paused(&self) -> bool {
        self.pool_state.load(Ordering::Acquire) & Self::PAUSED_BIT != 0
    }

    /// Sets the pool as paused.
    pub fn pause(&self) {
        self.pool_state
            .fetch_or(Self::PAUSED_BIT, Ordering::Release);
    }

    /// Resumes the pool and wakes all waiting clients.
    pub fn resume(&self) {
        self.pool_state
            .fetch_and(!Self::PAUSED_BIT, Ordering::Release);
        self.resume_notify.notify_waiters();
    }

    /// Returns a future that completes when the pool is resumed.
    pub fn resume_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.resume_notify.notified()
    }

    /// Returns the current reconnect epoch.
    pub fn current_epoch(&self) -> u32 {
        (self.pool_state.load(Ordering::Acquire) & Self::EPOCH_MASK) as u32
    }

    /// Increments the reconnect epoch and returns the new value.
    /// Uses CAS loop to modify only the lower 32 bits, preventing
    /// epoch overflow from corrupting PAUSED_BIT at bit 32.
    pub fn bump_epoch(&self) -> u32 {
        loop {
            let old = self.pool_state.load(Ordering::Acquire);
            let old_epoch = (old & Self::EPOCH_MASK) as u32;
            let new_epoch = old_epoch.wrapping_add(1);
            let new = (old & !Self::EPOCH_MASK) | (new_epoch as u64);
            if self
                .pool_state
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return new_epoch;
            }
        }
    }

    /// Checks whether a server connection can be reused.
    ///
    /// `skip_lifetime` suppresses only `server_lifetime` under client
    /// pressure. Bad-connection, reconnect-epoch, and alive checks still run.
    pub async fn recycle(
        &self,
        conn: &mut Server,
        metrics: &Metrics,
        skip_lifetime: bool,
    ) -> RecycleResult {
        if conn.is_bad() {
            conn.close_reason = Some("bad connection".to_string());
            return Err(RecycleError::StaticMessage("Bad connection"));
        }

        // RECONNECT epoch check: reject connections created before current epoch
        if metrics.epoch < self.current_epoch() {
            conn.close_reason = Some("reconnect epoch outdated".to_string());
            return Err(RecycleError::StaticMessage(
                "Connection outdated (RECONNECT)",
            ));
        }

        // Lifetime cleanup is skipped while the pool is under pressure.
        if let Some(age_ms) = lifetime_exceeded(metrics, skip_lifetime) {
            conn.close_reason = Some(format!(
                "lifetime exceeded (age={}, limit={})",
                format_duration_ms(age_ms),
                format_duration_ms(metrics.lifetime_ms),
            ));
            return Err(RecycleError::StaticMessage("Connection exceeded lifetime"));
        }

        // Probe long-idle connections before reuse.
        if self.idle_check_timeout_ms > 0 {
            if let Some(recycled) = metrics.recycled {
                let idle_time_ms = recycled.elapsed().as_millis() as u64;
                if idle_time_ms > self.idle_check_timeout_ms {
                    debug!("Connection {conn} idle for {idle_time_ms}ms, checking alive...");
                    if conn.check_alive(self.connect_timeout).await.is_err() {
                        conn.close_reason = Some(format!(
                            "failed alive check after {} idle",
                            format_duration_ms(idle_time_ms),
                        ));
                        return Err(RecycleError::StaticMessage("Connection failed alive check"));
                    }
                    debug!("Connection {conn} passed alive check");
                }
            }
        }

        Ok(())
    }
}

/// Compact "host:port(role)" list for fallback wave logs.
fn format_target_list(targets: &[super::fallback::FallbackTarget]) -> String {
    targets
        .iter()
        .map(|t| format!("{}:{}({:?})", t.host, t.port, t.role))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_backend_unreachable(err: &Error) -> bool {
    matches!(
        err,
        Error::ConnectError(_) | Error::ServerUnavailableError(_, _)
    )
}

/// Per-candidate failure counters and typed host-independent errors.
#[derive(Default)]
struct FailureSummary {
    last_err: Option<Error>,
    /// First startup-parameter rejection seen in this fallback round.
    typed_startup_rejection: Option<Error>,
    /// First local fd exhaustion seen in this fallback round.
    typed_resource_exhaustion: Option<Error>,
    counts: std::collections::HashMap<super::fallback::FailureReason, u32>,
}

impl FailureSummary {
    fn record(&mut self, err: Error, reason: super::fallback::FailureReason) {
        *self.counts.entry(reason).or_insert(0) += 1;
        if matches!(
            reason,
            super::fallback::FailureReason::StartupParameterRejection
        ) && self.typed_startup_rejection.is_none()
        {
            // Keep a typed copy; the original moves into `last_err`.
            if let Error::ServerStartupParameterRejection {
                sqlstate,
                message,
                server_identifier,
            } = &err
            {
                self.typed_startup_rejection = Some(Error::ServerStartupParameterRejection {
                    sqlstate: sqlstate.clone(),
                    message: message.clone(),
                    server_identifier: server_identifier.clone(),
                });
            }
        }
        if matches!(reason, super::fallback::FailureReason::ResourceExhausted)
            && self.typed_resource_exhaustion.is_none()
        {
            self.typed_resource_exhaustion = Some(err.clone());
        }
        self.last_err = Some(err);
    }

    /// Whether the round saw a typed startup-parameter rejection.
    fn has_startup_parameter_rejection(&self) -> bool {
        self.typed_startup_rejection.is_some()
    }

    fn startup_rejection(&self) -> Option<Error> {
        self.typed_startup_rejection.clone()
    }

    #[cfg(test)]
    fn into_startup_rejection(self) -> Option<Error> {
        self.typed_startup_rejection
    }

    fn has_resource_exhaustion(&self) -> bool {
        self.typed_resource_exhaustion.is_some()
    }

    fn resource_exhaustion(&self) -> Option<Error> {
        self.typed_resource_exhaustion.clone()
    }

    #[cfg(test)]
    fn into_resource_exhaustion(self) -> Option<Error> {
        self.typed_resource_exhaustion
    }

    /// Legacy helper for tests that require every failure to be a rejection.
    #[cfg(test)]
    fn all_startup_parameter_rejection(&self) -> bool {
        !self.counts.is_empty()
            && self
                .counts
                .keys()
                .all(|r| matches!(r, super::fallback::FailureReason::StartupParameterRejection))
    }

    #[cfg(test)]
    fn into_last_err(self) -> Option<Error> {
        self.last_err
    }

    fn format(&self) -> String {
        if self.counts.is_empty() {
            return "no candidates".to_string();
        }
        // Stable ordering so the message is deterministic in logs and tests.
        let mut parts: Vec<(super::fallback::FailureReason, u32)> =
            self.counts.iter().map(|(r, c)| (*r, *c)).collect();
        parts.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        parts
            .into_iter()
            .map(|(r, c)| format!("{c} {}", r.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Return the first success with its original index.
/// If every future fails, return all indexed errors.
async fn race_first_success<'a, T: 'a, E: 'a>(
    futures: Vec<futures::future::BoxFuture<'a, Result<T, E>>>,
) -> Result<(T, usize), Vec<(usize, E)>> {
    if futures.is_empty() {
        return Err(Vec::new());
    }

    // Keep original indices; select_all renumbers as `rest` shrinks.
    let mut indexed: Vec<futures::future::BoxFuture<'a, (usize, Result<T, E>)>> = futures
        .into_iter()
        .enumerate()
        .map(|(i, f)| Box::pin(async move { (i, f.await) }) as _)
        .collect();

    let mut errors: Vec<(usize, E)> = Vec::new();
    while !indexed.is_empty() {
        let ((idx, result), _, rest) = futures::future::select_all(indexed).await;
        match result {
            Ok(value) => return Ok((value, idx)),
            Err(e) => errors.push((idx, e)),
        }
        indexed = rest;
    }

    Err(errors)
}

/// Wrap server startup in the fallback connect timeout.
/// Timeouts become `ConnectError` so fallback can try another candidate.
async fn startup_with_timeout<F>(
    timeout_duration: Duration,
    host: &str,
    port: u16,
    fut: F,
) -> Result<Server, Error>
where
    F: std::future::Future<Output = Result<Server, Error>>,
{
    match tokio::time::timeout(timeout_duration, fut).await {
        Ok(result) => result,
        Err(_) => Err(Error::ConnectError(format!(
            "server startup timed out to {}:{} after {}ms",
            host,
            port,
            timeout_duration.as_millis()
        ))),
    }
}

/// Return age when per-connection lifetime is exceeded and not skipped.
fn lifetime_exceeded(metrics: &Metrics, skip_lifetime: bool) -> Option<u64> {
    if skip_lifetime || metrics.lifetime_ms == 0 {
        return None;
    }
    let age_ms = metrics.age().as_millis() as u64;
    if age_ms > metrics.lifetime_ms {
        Some(age_ms)
    } else {
        None
    }
}

/// Error classes that a different Patroni member cannot fix.
///
/// Keep this predicate aligned with `is_backend_unreachable` and
/// `FailureReason::warrants_host_cooldown`: local fd exhaustion and
/// operator startup-parameter errors must not clear the whitelist or
/// enter host cooldown bookkeeping.
fn is_host_independent_error(e: &Error) -> bool {
    matches!(
        e,
        Error::ServerStartupParameterRejection { .. } | Error::ConnectResourceExhausted(_)
    )
}

fn record_fallback_connection_outcome(pool_name: &str, established: bool) {
    if established {
        crate::web::metrics::record_pool_only_fallback_tls_label(pool_name);
        crate::web::metrics::FALLBACK_CONNECTIONS_TOTAL
            .with_label_values(&[pool_name])
            .inc();
    }
}

struct ServerStatsRegistrationGuard {
    stats: Arc<ServerStats>,
    armed: bool,
}

impl ServerStatsRegistrationGuard {
    fn register(stats: Arc<ServerStats>) -> Self {
        stats.register(stats.clone());
        Self { stats, armed: true }
    }

    fn disconnect(mut self) {
        if self.armed {
            self.stats.disconnect();
            self.armed = false;
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ServerStatsRegistrationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.stats.disconnect();
            self.armed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn metrics_with_lifetime(lifetime_ms: u64) -> Metrics {
        Metrics::new(lifetime_ms, 0, 0)
    }

    fn test_server_pool_with_prewarm(prewarm_query: &str) -> ServerPool {
        ServerPool::new(
            crate::config::Address::default(),
            crate::config::User::default(),
            "test_db",
            Arc::new(dashmap::DashMap::new()),
            false,
            false,
            0,
            "test_app".to_string(),
            1,
            60_000,
            60_000,
            60_000,
            Duration::from_secs(5),
            Duration::from_secs(5),
            false,
            None,
            Arc::new(std::collections::BTreeMap::new()),
            Arc::new(std::collections::BTreeMap::new()),
        )
        .with_prewarm_query(prewarm_query.to_string())
    }

    fn rejection_err() -> Error {
        Error::ServerStartupParameterRejection {
            sqlstate: "22023".to_string(),
            message: "invalid_value".to_string(),
            server_identifier: crate::app::errors::ServerIdentifier::new(
                "alice".to_string(),
                "db",
                "pool_a",
            ),
        }
    }

    fn timeout_err() -> Error {
        Error::ConnectError("server startup timed out after 5s".to_string())
    }

    fn resource_exhausted_err() -> Error {
        Error::ConnectResourceExhausted("too many open files".to_string())
    }

    #[test]
    fn is_host_independent_error_covers_resource_exhaustion_and_startup_rejection() {
        assert!(is_host_independent_error(&resource_exhausted_err()));
        assert!(is_host_independent_error(&rejection_err()));
    }

    #[test]
    fn is_host_independent_error_excludes_transport_failures() {
        let id = crate::errors::ServerIdentifier::new("alice".to_string(), "db", "pool_a");
        assert!(
            !is_host_independent_error(&Error::ConnectError("connection refused".into())),
            "transport failures are candidate-specific"
        );
        assert!(
            !is_host_independent_error(&timeout_err()),
            "startup timeout is candidate-specific"
        );
        assert!(
            !is_host_independent_error(&Error::ServerUnavailableError(
                "the database system is starting up".into(),
                id,
            )),
            "server unavailable is candidate-specific"
        );
    }

    #[test]
    fn backend_unreachable_excludes_local_resource_exhaustion() {
        let id = crate::errors::ServerIdentifier::new("alice".to_string(), "db", "pool_a");

        assert!(is_backend_unreachable(&Error::ConnectError(
            "connection refused".into()
        )));
        assert!(is_backend_unreachable(&Error::ServerUnavailableError(
            "the database system is starting up".into(),
            id,
        )));
        assert!(
            !is_backend_unreachable(&resource_exhausted_err()),
            "EMFILE/ENFILE is pg_doorman resource exhaustion, not backend unreachability"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canceling_prewarm_query_counts_failure() {
        use std::sync::atomic::Ordering;
        use tokio::io::AsyncReadExt;

        let pool = test_server_pool_with_prewarm("SELECT pg_sleep(60)");
        let (mut server, mut peer) = crate::server::Server::test_silent_socket();

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            pool.run_prewarm_query(&mut server),
        )
        .await;
        assert!(
            result.is_err(),
            "silent peer must keep prewarm pending until the outer create timeout cancels it"
        );

        let mut observed = [0_u8; 64];
        let n = tokio::time::timeout(Duration::from_millis(50), peer.read(&mut observed))
            .await
            .expect("test peer must observe the prewarm query write before cancellation")
            .expect("reading from test peer must succeed");
        assert!(
            n > 0,
            "test precondition: prewarm bytes must reach the peer"
        );

        assert_eq!(
            pool.address
                .stats
                .prewarm_failures_total
                .load(Ordering::Relaxed),
            1,
            "cancelled prewarm must be counted as a prewarm failure"
        );
    }

    #[test]
    fn prewarm_query_requires_recycle_safe_backend_before_publish() {
        let src = include_str!("server_pool.rs");
        let start = src
            .find("async fn run_prewarm_query")
            .expect("run_prewarm_query must exist");
        let block = &src[start..];
        let end = block
            .find("/// See `operator_managed_startup_keys` field.")
            .expect("run_prewarm_query block should end before operator key accessor");
        let block = &block[..end];

        assert!(
            block.contains("recycle_safety_violation"),
            "successful prewarm_query must prove the backend is idle/clean before publishing it"
        );
        assert!(
            block.contains("conn.mark_bad"),
            "a backend that is not recycle-safe after prewarm must be marked bad"
        );
        assert!(
            block.contains("prewarm_failure"),
            "post-prewarm recycle-safety failures must increment prewarm failure stats"
        );
    }

    #[test]
    fn all_startup_parameter_rejection_false_when_empty() {
        let s = FailureSummary::default();
        assert!(
            !s.all_startup_parameter_rejection(),
            "empty summary must not claim everyone rejected on startup parameter"
        );
    }

    #[test]
    fn all_startup_parameter_rejection_true_when_only_rejections() {
        let mut s = FailureSummary::default();
        s.record(
            rejection_err(),
            super::super::fallback::FailureReason::StartupParameterRejection,
        );
        s.record(
            rejection_err(),
            super::super::fallback::FailureReason::StartupParameterRejection,
        );
        assert!(s.all_startup_parameter_rejection());
    }

    #[test]
    fn all_startup_parameter_rejection_false_when_mixed() {
        let mut s = FailureSummary::default();
        s.record(
            rejection_err(),
            super::super::fallback::FailureReason::StartupParameterRejection,
        );
        s.record(
            timeout_err(),
            super::super::fallback::FailureReason::Timeout,
        );
        assert!(
            !s.all_startup_parameter_rejection(),
            "a single non-rejection cause must veto the all-rejection shortcut"
        );
        // But the typed rejection must still be retained so a mixed
        // round of transport failures + one PG rejection surfaces the
        // PG SQLSTATE/message instead of the generic aggregate.
        assert!(
            s.has_startup_parameter_rejection(),
            "a single PG rejection must be retained even alongside transport failures"
        );
    }

    #[test]
    fn into_startup_rejection_returns_first_typed_rejection() {
        let mut s = FailureSummary::default();
        s.record(
            timeout_err(),
            super::super::fallback::FailureReason::Timeout,
        );
        s.record(
            rejection_err(),
            super::super::fallback::FailureReason::StartupParameterRejection,
        );
        match s.into_startup_rejection() {
            Some(Error::ServerStartupParameterRejection { sqlstate, .. }) => {
                assert_eq!(sqlstate, "22023")
            }
            other => panic!("expected the retained rejection, got {other:?}"),
        }
    }

    #[test]
    fn failure_summary_retains_resource_exhaustion() {
        let mut s = FailureSummary::default();
        s.record(
            timeout_err(),
            super::super::fallback::FailureReason::Timeout,
        );
        s.record(
            resource_exhausted_err(),
            super::super::fallback::FailureReason::ResourceExhausted,
        );

        assert!(s.has_resource_exhaustion());
        match s.into_resource_exhaustion() {
            Some(Error::ConnectResourceExhausted(msg)) => {
                assert!(msg.contains("too many open files"), "unexpected msg: {msg}");
            }
            other => panic!("expected resource exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn into_last_err_returns_most_recently_recorded() {
        let mut s = FailureSummary::default();
        s.record(
            timeout_err(),
            super::super::fallback::FailureReason::Timeout,
        );
        s.record(
            rejection_err(),
            super::super::fallback::FailureReason::StartupParameterRejection,
        );
        match s.into_last_err() {
            Some(Error::ServerStartupParameterRejection { sqlstate, .. }) => {
                assert_eq!(sqlstate, "22023")
            }
            other => panic!("expected the last-recorded rejection, got {other:?}"),
        }
    }

    #[test]
    fn lifetime_exceeded_skipped_when_under_pressure() {
        // A connection well past its budget is kept alive when the caller
        // signals pressure. A working connection must not be closed mid-storm.
        let metrics = metrics_with_lifetime(1);
        thread::sleep(Duration::from_millis(5));
        assert!(lifetime_exceeded(&metrics, true).is_none());
    }

    #[test]
    fn lifetime_exceeded_returns_age_when_age_above_limit() {
        let metrics = metrics_with_lifetime(1);
        // 1ms budget with ±20% jitter resolves to 1ms exactly (jitter floor).
        // Sleep well past it, then assert we report the breach.
        thread::sleep(Duration::from_millis(5));
        let age = lifetime_exceeded(&metrics, false).expect("must exceed lifetime");
        assert!(age >= 1, "reported age {age} must be > limit 1");
    }

    #[test]
    fn lifetime_exceeded_none_when_lifetime_disabled() {
        // lifetime_ms == 0 means "no budget, never expire", and that
        // contract must hold regardless of the skip flag.
        let metrics = metrics_with_lifetime(0);
        thread::sleep(Duration::from_millis(2));
        assert!(lifetime_exceeded(&metrics, false).is_none());
        assert!(lifetime_exceeded(&metrics, true).is_none());
    }

    #[test]
    fn lifetime_exceeded_none_when_age_within_limit() {
        // Generous budget — connection is fresh, no breach reported.
        let metrics = metrics_with_lifetime(60_000);
        assert!(lifetime_exceeded(&metrics, false).is_none());
    }

    #[tokio::test]
    async fn startup_with_timeout_returns_connect_error_on_deadline() {
        // Simulates a server that opened TCP but never replies to
        // StartupMessage: the inner future never resolves. We expect
        // `startup_with_timeout` to return `ConnectError`, which callers treat
        // as a transport-level failure (triggers fallback on the main path;
        // marks the candidate unhealthy on the fallback path).
        let pending = std::future::pending::<Result<Server, Error>>();
        let result =
            startup_with_timeout(Duration::from_millis(20), "1.2.3.4", 5432, pending).await;

        match result {
            Err(Error::ConnectError(msg)) => {
                assert!(
                    msg.contains("startup timed out") && msg.contains("1.2.3.4:5432"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected ConnectError, got: {other:?}"),
        }
    }

    #[test]
    fn failure_summary_format_aggregates_by_reason() {
        use super::super::fallback::FailureReason;
        let mut s = FailureSummary::default();
        s.record(
            Error::ServerStartupError(
                "auth fail".into(),
                crate::errors::ServerIdentifier::new("u".into(), "d", "p"),
            ),
            FailureReason::StartupError,
        );
        s.record(
            Error::ServerStartupError(
                "auth fail".into(),
                crate::errors::ServerIdentifier::new("u".into(), "d", "p"),
            ),
            FailureReason::StartupError,
        );
        s.record(
            Error::ConnectError("timed out".into()),
            FailureReason::Timeout,
        );

        let out = s.format();
        // Stable alphabetic order by reason.as_str(): startup_error < timeout.
        assert_eq!(out, "2 startup_error, 1 timeout");
    }

    #[test]
    fn failure_summary_format_no_candidates_when_empty() {
        let s = FailureSummary::default();
        assert_eq!(s.format(), "no candidates");
    }

    #[test]
    fn fallback_connection_counter_only_records_established_connections() {
        let pool = "test_pool_fallback_established_counter";
        let before = crate::web::metrics::FALLBACK_CONNECTIONS_TOTAL
            .with_label_values(&[pool])
            .get();

        record_fallback_connection_outcome(pool, false);
        assert_eq!(
            crate::web::metrics::FALLBACK_CONNECTIONS_TOTAL
                .with_label_values(&[pool])
                .get()
                - before,
            0
        );

        record_fallback_connection_outcome(pool, true);
        assert_eq!(
            crate::web::metrics::FALLBACK_CONNECTIONS_TOTAL
                .with_label_values(&[pool])
                .get()
                - before,
            1
        );
    }

    #[test]
    fn server_stats_registration_guard_disconnects_on_drop_until_disarmed() {
        let pool = "test_pool_stats_registration_guard";
        let address = crate::config::Address {
            pool_name: pool.to_string(),
            ..crate::config::Address::default()
        };
        let stats = Arc::new(ServerStats::new(address, crate::utils::clock::now()));

        {
            let _guard = ServerStatsRegistrationGuard::register(stats.clone());
            assert!(
                crate::stats::get_server_stats()
                    .values()
                    .any(|registered| registered.pool_name() == pool),
                "guard must publish stats while startup is in flight"
            );
        }
        assert!(
            !crate::stats::get_server_stats()
                .values()
                .any(|registered| registered.pool_name() == pool),
            "dropping an armed guard must remove the in-flight stats"
        );

        let guard = ServerStatsRegistrationGuard::register(stats.clone());
        guard.disarm();
        assert!(
            crate::stats::get_server_stats()
                .values()
                .any(|registered| registered.pool_name() == pool),
            "disarmed guard hands lifecycle ownership to Server"
        );
        stats.disconnect();
    }

    #[tokio::test]
    async fn startup_with_timeout_passes_through_when_inner_resolves() {
        // Successful inner future must not be modified by the wrapper.
        let inner = async {
            Err::<Server, _>(Error::ConnectError(
                "deliberate inner error to assert pass-through".into(),
            ))
        };
        let result = startup_with_timeout(Duration::from_secs(1), "1.2.3.4", 5432, inner).await;

        match result {
            Err(Error::ConnectError(msg)) => {
                assert!(msg.contains("pass-through"), "unexpected message: {msg}");
            }
            other => panic!("expected pass-through ConnectError, got: {other:?}"),
        }
    }

    // -- race_first_success --------------------------------------------------

    use futures::future::BoxFuture;

    #[tokio::test]
    async fn race_first_success_returns_first_ok_with_index() {
        // The early candidate yields a slow Err, the second yields Ok
        // immediately. Winner index must be 1, value must come from the
        // second future. Pending third future is dropped — required so the
        // test does not stall the runtime for a minute.
        let f0: BoxFuture<'static, Result<&str, &str>> = Box::pin(async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            Err("late err")
        });
        let f1: BoxFuture<'static, Result<&str, &str>> = Box::pin(async { Ok("won") });
        let f2: BoxFuture<'static, Result<&str, &str>> = Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok("never reached")
        });

        let (val, idx) = race_first_success(vec![f0, f1, f2])
            .await
            .expect("expected Ok");
        assert_eq!(val, "won");
        assert_eq!(idx, 1);
    }

    #[tokio::test]
    async fn race_first_success_collects_all_errors_when_all_fail() {
        // Every candidate errors. All errors must be collected with their
        // original indices so the caller can mark each candidate unhealthy
        // and aggregate the reasons in the exhaustion log.
        let f0: BoxFuture<'static, Result<&str, &str>> = Box::pin(async { Err("e0") });
        let f1: BoxFuture<'static, Result<&str, &str>> = Box::pin(async { Err("e1") });
        let f2: BoxFuture<'static, Result<&str, &str>> = Box::pin(async { Err("e2") });

        let errs = race_first_success(vec![f0, f1, f2])
            .await
            .expect_err("expected Err");
        assert_eq!(errs.len(), 3);
        let mut indices: Vec<usize> = errs.iter().map(|(i, _)| *i).collect();
        indices.sort();
        assert_eq!(indices, vec![0, 1, 2]);
        // Errors stay attached to their original index, regardless of the
        // order they completed in.
        for (idx, err) in &errs {
            assert_eq!(*err, ["e0", "e1", "e2"][*idx]);
        }
    }

    #[tokio::test]
    async fn race_first_success_empty_input() {
        // Vacuous case: caller must not get a panic on a zero-candidate
        // wave (happens when wave 1 has no sync_standby members).
        let errs: Vec<(usize, &str)> = race_first_success::<&str, &str>(vec![])
            .await
            .expect_err("expected Err");
        assert!(errs.is_empty(), "no candidates → no errors");
    }

    #[tokio::test]
    async fn race_first_success_first_ok_immediately() {
        // First-completed future is the winner even when the second would
        // also have succeeded later.
        let f0: BoxFuture<'static, Result<&str, &str>> = Box::pin(async { Ok("first") });
        let f1: BoxFuture<'static, Result<&str, &str>> = Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok("late")
        });

        let (val, idx) = race_first_success(vec![f0, f1]).await.expect("expected Ok");
        assert_eq!(val, "first");
        assert_eq!(idx, 0);
    }
}
