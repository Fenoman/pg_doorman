use arc_swap::ArcSwap;
use dashmap::DashMap;
use log::{debug, info};
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{
    get_config, tls, Address, BackendAuthMethod, Config, General, Pool as ConfigPool, PoolMode,
    User,
};
use crate::errors::Error;
use crate::messages::Parse;

use crate::server::ServerParameters;
use crate::stats::auth_query::AuthQueryStats;
use crate::stats::AddressStats;

mod errors;
mod inner;
mod types;

pub use errors::{PoolError, RecycleError, RecycleResult};
pub use inner::{Object, Pool, PoolBuilder, ScalingStatsSnapshot};
pub use types::{Metrics, PoolConfig, QueueMode, ScalingConfig, Status, Timeouts};

pub use crate::server::PreparedStatementCache;

mod auth_query_state;
mod check_query_cache;
mod dynamic;
mod eviction;
pub mod gc;
mod init_guard;
pub mod pool_coordinator;
pub mod retain;
mod server_pool;
pub mod startup_resolver;

pub mod fallback;

pub use auth_query_state::AuthQueryState;
pub use check_query_cache::CheckQueryCache;
pub use dynamic::create_dynamic_pool;
pub use eviction::PoolEvictionSource;
pub use init_guard::PoolInitGuard;
pub use server_pool::ServerPool;

pub type ProcessId = i32;
pub type SecretKey = i32;
pub type ServerHost = String;
pub type ServerPort = u16;

/// Target information for forwarding a CancelRequest to the correct backend.
#[derive(Debug, Clone)]
pub struct CancelTarget {
    pub process_id: ProcessId,
    pub secret_key: SecretKey,
    pub host: ServerHost,
    pub port: ServerPort,
    pub server_tls: Arc<tls::ServerTlsConfig>,
    pub connected_with_tls: bool,
    pub pool_name: String,
}

pub type ClientServerMap = Arc<DashMap<(ProcessId, SecretKey), CancelTarget>>;
pub type PoolMap = HashMap<PoolIdentifier, ConnectionPool>;

/// The connection pool, globally available.
/// This is atomic and safe and read-optimized.
/// The pool is recreated dynamically when the config is reloaded.
pub static POOLS: Lazy<ArcSwap<PoolMap>> = Lazy::new(|| ArcSwap::from_pointee(HashMap::default()));

/// Hash of the previous reload's `general.startup_parameters` map. Used by
/// `ConnectionPool::from_config` to recognize when a SIGHUP changed the
/// general-level baseline so dynamic auth_query pools can be drained — the
/// per-pool reuse hash already folds in the baseline, but dynamic pools are
/// carried over by identifier rather than rebuilt from the same path.
static PREVIOUS_GENERAL_STARTUP_HASH: AtomicU64 = AtomicU64::new(0);
/// Process IDs of PostgreSQL backends that received an out-of-band
/// CancelRequest. The set is consulted on every server checkout (so a
/// backend that was canceled mid-query is evicted rather than handed to
/// a different client) and on every `Server::Drop`. Cancel requests are
/// rare relative to checkouts.
///
/// earlier a `Lazy<Arc<Mutex<HashSet<ProcessId>>>>` - a single
/// global `parking_lot::Mutex` taken on every checkout serialised the
/// entire fleet through one cache line. Migrated to a sharded set so
/// the hot read path (contains-check during checkout) and the hot write
/// path (Server::Drop) operate on independent shards.
///
/// Quarantine markers for backend PG pids with a forwarded cancel in flight.
/// `DashMap<pid, Instant>` - the value is the insertion time so the checkout
/// consume (`canceled_pids_consume`) can distinguish a FRESH marker (a cancel
/// may still be landing -> evict the backend) from a STALE one (the OS pid was
/// recycled by PG to a healthy NEW backend -> reuse it, the recycled-pid quarantine fix). Lock-free
/// sharded; same structure as the former `DashMap`, now carrying a timestamp
/// instead of unit.
pub static CANCELED_PIDS: Lazy<Arc<dashmap::DashMap<ProcessId, Instant>>> =
    Lazy::new(|| Arc::new(dashmap::DashMap::new()));

/// hard cap on `CANCELED_PIDS` to bound the
/// worst-case memory footprint. Three real paths leak stale entries
/// forever without this cap:
/// - Cancel for a pid the retain loop has already evicted as dead.
/// - Cancel arriving during graceful shutdown (the matching backend
///   was already marked bad and Drop'd; F10's bad-gate cleared it,
///   but the cancel-side insert can race past the clear).
/// - Cancel for a pid that pg_doorman never assigned (libpq retries
///   on its own when the original cancel TCP fails).
///
/// 65536 entries × 4 bytes (i32 pid) + DashMap overhead ≈ 4-6 MB.
/// At that scale the operator should already be paging on the
/// `canceled_pids_cap_dropped` counter and the system is in pathological
/// territory anyway - dropping the latest cancel signal is the right
/// trade compared to unbounded growth.
const CANCELED_PIDS_CAP: usize = 65536;

/// TTL for a quarantine marker. A marker only needs to
/// outlive the async cancel pipeline (bounded by `CANCEL_PIPELINE_FLOOR` = 5s
/// in startup_cancel.rs); once it lands or fails, reusing the backend is safe.
/// Beyond this TTL a surviving marker is stale - `Server::Drop` keeps the entry
/// on a clean (`!bad`) drop, and PG can recycle that OS pid to a healthy NEW
/// backend - so it must NOT evict that backend. 60s is a 12× margin over the 5s
/// pipeline floor.
const CANCELED_PIDS_TTL: Duration = Duration::from_secs(60);

/// insert helper that respects `CANCELED_PIDS_CAP` and
/// emits a `warn!` when the cap kicks in so operators can detect the
/// leak path. Returns `true` if the insert actually happened.
pub fn canceled_pids_insert(pid: ProcessId) -> bool {
    // Length is best-effort across shards (DashMap does not lock the
    // whole map for `len`); racing inserts can take the count slightly
    // over the cap by a sub-percentage. That is acceptable - the cap
    // exists to bound the order of magnitude, not enforce an exact
    // limit.
    if CANCELED_PIDS.len() >= CANCELED_PIDS_CAP {
        log::warn!(
            "CANCELED_PIDS hit cap {CANCELED_PIDS_CAP}; dropping cancel signal for pid={pid}. \
             Investigate cancel-storm sources or backend churn."
        );
        return false;
    }
    CANCELED_PIDS.insert(pid, Instant::now());
    true
}

/// fail-closed decision point for forwarding a client cancel
/// request to the backend. Sets the quarantine marker for `pid` and
/// returns whether the cancel may be forwarded.
///
/// The quarantine marker is what makes a forwarded cancel safe in
/// `pool_mode=transaction`: a backend whose pid is in `CANCELED_PIDS`
/// is evicted on check-in instead of being recycled to the next
/// client. If `canceled_pids_insert` fails (the set is at
/// `CANCELED_PIDS_CAP`), the marker is NOT set, so forwarding the
/// cancel would let the backend be handed to a different client
/// before the async cancel TCP connection lands - cancelling that
/// other client's query. In that case we return `false`: the caller
/// must DROP the cancel (the client's Ctrl-C becomes a safe no-op)
/// rather than forward it.
pub fn should_forward_cancel(pid: ProcessId) -> bool {
    canceled_pids_insert(pid)
}

/// Outcome of consuming a `CANCELED_PIDS` quarantine marker at checkout.
#[derive(Debug, PartialEq, Eq)]
pub enum CancelMarker {
    /// No marker for this pid - reuse the backend normally.
    Absent,
    /// Fresh marker: a cancel for this backend may still be in flight - evict
    /// it so a late cancel cannot land on the next client's query.
    Fresh,
    /// Stale marker (older than `CANCELED_PIDS_TTL`): the cancel has long since
    /// landed or failed, so the OS pid was almost certainly recycled by PG to
    /// this healthy NEW backend. The entry is cleared and the backend reused -
    /// the recycled-pid quarantine fix (no needless eviction of a recycled-pid backend).
    Stale,
}

/// Consume (remove) the quarantine marker for `pid`, reporting whether it was
/// FRESH (evict the backend) or STALE (recycled pid - reuse it). Always clears
/// any entry so a stale marker cannot linger across pid recycling.
pub fn canceled_pids_consume(pid: ProcessId) -> CancelMarker {
    canceled_pids_consume_with_ttl(pid, CANCELED_PIDS_TTL)
}

/// Inner of [`canceled_pids_consume`] with the TTL injected so tests can pick
/// the FRESH vs STALE branch deterministically without sleeping.
fn canceled_pids_consume_with_ttl(pid: ProcessId, ttl: Duration) -> CancelMarker {
    match CANCELED_PIDS.remove(&pid) {
        None => CancelMarker::Absent,
        Some((_, marked_at)) => {
            if marked_at.elapsed() < ttl {
                CancelMarker::Fresh
            } else {
                CancelMarker::Stale
            }
        }
    }
}

/// Per-database pool coordinators, keyed by pool name.
/// Created in `from_config()` for pools with `max_db_connections > 0`.
/// Replaced atomically on RELOAD. When a coordinator is replaced, old connections
/// that hold permits from the previous coordinator continue working until they
/// are naturally closed — the old `Arc<PoolCoordinator>` lives as long as its
/// permits do.
pub static COORDINATORS: Lazy<ArcSwap<HashMap<String, Arc<pool_coordinator::PoolCoordinator>>>> =
    Lazy::new(|| ArcSwap::from_pointee(HashMap::new()));

/// Serialises every mutation across `POOLS` / `DYNAMIC_POOLS` /
/// `COORDINATORS` / `AUTH_QUERY_STATE`. The four ArcSwaps are
/// individually atomic but a `load -> clone -> mutate -> store` sequence
/// is not, so two concurrent writers - RELOAD vs. `create_dynamic_pool`,
/// two concurrent dynamic-pool inserts for different users, GC vs.
/// `register_dynamic_pool`, etc. - could drop one writer's update
/// (last-write-wins). `from_config` also needs cross-store atomicity
/// across the four globals, which a single `ArcSwap::rcu` cannot
/// provide.
///
/// Hot read paths (`get_pool`, `is_dynamic_pool`, `get_coordinator`,
/// `get_auth_query_state`) keep using `ArcSwap::load` and are unchanged -
/// this lock guards the write side only. Write contention is bounded:
/// RELOAD is rare (SIGHUP / admin RELOAD), dynamic-pool register/drop
/// fires at first per-user login or after auth_query refetch, GC sweep
/// runs on a slow ticker. Always acquire via `pool_write_lock()` so the
/// hold window stays around the load/clone/store sequence and never
/// covers IO.
static RELOAD_LOCK: Lazy<parking_lot::Mutex<()>> = Lazy::new(|| parking_lot::Mutex::new(()));

/// serialise the entire `from_config` body across the
/// async build phase. The `RELOAD_LOCK` only protects the COMMIT
/// step (store calls), leaving the build phase racy: two SIGHUPs or
/// SIGHUP+admin-RELOAD that overlap can both load the same `POOLS`
/// baseline, both build divergent `new_pools`, both queue at
/// `pool_write_lock()`, and the second commits its snapshot
/// overwriting the first - silently losing any user/pool the first
/// publisher added between the two configs. Using a `tokio::sync::Mutex`
/// (acceptable because RELOAD is a slow admin operation, never on hot
/// path) serialises the whole reload sequence including IO.
pub(crate) static RELOAD_BUILD_LOCK: Lazy<tokio::sync::Mutex<()>> =
    Lazy::new(|| tokio::sync::Mutex::new(()));

/// Acquire the write-serialisation lock for the POOLS family. The guard
/// must be held for the entire `load -> clone -> mutate -> store`
/// sequence (and across multiple stores when mutating more than one
/// global). Build new pools and fallible config dependencies outside
/// the lock; the final dynamic-pool reconciliation and publish step
/// need to be inside.
#[must_use = "the guard must be held for the load->clone->store sequence"]
pub(crate) fn pool_write_lock() -> parking_lot::MutexGuard<'static, ()> {
    RELOAD_LOCK.lock()
}

/// Global client-server map, initialized once by `from_config()`.
/// Needed by `create_dynamic_pool()` which doesn't have access to the map
/// through function parameters.
static CLIENT_SERVER_MAP: OnceCell<ClientServerMap> = OnceCell::new();

fn set_client_server_map(csm: ClientServerMap) {
    CLIENT_SERVER_MAP.set(csm).ok();
}

pub fn get_client_server_map() -> Option<ClientServerMap> {
    CLIENT_SERVER_MAP.get().cloned()
}

/// Stable hash of a per-user auth_query `startup_parameters` overlay.
/// Used to detect overlay drift after `auth_query` refetches: if the
/// new row's hash differs from `ConnectionPool::per_user_startup_overlay_hash`,
/// the dynamic pool is dropped so the next client connection rebuilds
/// against the new overlay. Accepts both `HashMap` (auth_query cache
/// shape) and `BTreeMap` (the immutable snapshot stored on the pool)
/// via a borrowed iterator, normalising key order so the hash is shape-
/// independent.
pub(crate) fn per_user_overlay_hash<'a, I>(entries: I) -> u64
where
    I: IntoIterator<Item = (&'a String, &'a String)>,
{
    use std::hash::{Hash, Hasher};
    let mut sorted: Vec<(&str, &str)> = entries
        .into_iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sorted.hash(&mut hasher);
    hasher.finish()
}

/// Hash that `per_user_overlay_hash` produces for the empty overlay.
/// Computed once and reused by every static / dedicated-mode pool so
/// drift comparisons against dynamic pools' real overlay hashes are
/// shape-stable across the codebase.
pub(crate) fn empty_overlay_hash() -> u64 {
    per_user_overlay_hash(std::iter::empty::<(&String, &String)>())
}

/// Build a `ServerTlsConfig` for a pool, merging pool-level overrides with general defaults.
pub(crate) fn build_server_tls_for_pool(
    pool_config: &ConfigPool,
    general: &General,
) -> Result<Arc<tls::ServerTlsConfig>, Error> {
    let mode_str = pool_config
        .server_tls_mode
        .as_deref()
        .unwrap_or(&general.server_tls_mode);
    let mode = mode_str.parse::<tls::ServerTlsMode>()?;

    let ca = pool_config
        .server_tls_ca_cert
        .as_ref()
        .or(general.server_tls_ca_cert.as_ref());
    let cert = pool_config
        .server_tls_certificate
        .as_ref()
        .or(general.server_tls_certificate.as_ref());
    let key = pool_config
        .server_tls_private_key
        .as_ref()
        .or(general.server_tls_private_key.as_ref());

    let config = tls::ServerTlsConfig::new(
        mode,
        ca.map(|s| std::path::Path::new(s.as_str())),
        cert.map(|s| std::path::Path::new(s.as_str())),
        key.map(|s| std::path::Path::new(s.as_str())),
    )?;

    Ok(Arc::new(config))
}

pub type PreparedStatementCacheType = Arc<PreparedStatementCache>;
/// write-once read-mostly holder for the backend's
/// startup ParameterStatus snapshot. The previous shape was
/// `Arc<tokio::sync::Mutex<ServerParameters>>`, which serialised every
/// `get_server_parameters().await` on the login path through a single
/// async mutex; under a reconnect storm (server restart, mass login)
/// the wait fanned out linearly with concurrency. `OnceCell` gives a
/// lock-free fast path on populated pools and a single-shot async init
/// for the very first caller, so the storm parallelises perfectly.
///
/// Replacement on RELOAD is handled implicitly: `from_config` builds a
/// fresh `ConnectionPool` (and therefore a fresh `OnceCell`) for every
/// (db, user) entry, so the cell is never "re-set" - it always starts
/// empty and fills exactly once per pool incarnation.
pub type ServerParametersType = Arc<tokio::sync::OnceCell<ServerParameters>>;

// AuthQueryState is in auth_query_state.rs, re-exported above.

/// Global auth_query state per database pool.
/// Replaced atomically on RELOAD together with POOLS.
pub static AUTH_QUERY_STATE: Lazy<ArcSwap<HashMap<String, Arc<AuthQueryState>>>> =
    Lazy::new(|| ArcSwap::from_pointee(HashMap::new()));

/// Tracks which pool identifiers were created dynamically (auth_query passthrough).
/// Used by RELOAD logic and GC to distinguish dynamic pools from static ones.
pub static DYNAMIC_POOLS: Lazy<ArcSwap<HashSet<PoolIdentifier>>> =
    Lazy::new(|| ArcSwap::from_pointee(HashSet::new()));

/// Register a pool identifier as dynamic (created by auth_query passthrough).
pub fn register_dynamic_pool(id: &PoolIdentifier) {
    // hold RELOAD_LOCK across load + store so a concurrent
    // register/GC/from_config cannot drop our insert via last-write-wins.
    // Fast pre-check avoids the lock for the steady-state case (pool
    // already registered).
    if DYNAMIC_POOLS.load().contains(id) {
        return;
    }
    let _guard = pool_write_lock();
    let current = DYNAMIC_POOLS.load();
    if current.contains(id) {
        return;
    }
    let mut new_set = (**current).clone();
    new_set.insert(id.clone());
    DYNAMIC_POOLS.store(Arc::new(new_set));
}

/// Check if a pool identifier is a dynamic (auth_query passthrough) pool.
pub fn is_dynamic_pool(id: &PoolIdentifier) -> bool {
    DYNAMIC_POOLS.load().contains(id)
}

/// Drop a dynamic pool from `POOLS` and `DYNAMIC_POOLS`, closing the removed
/// generation so stale auth_query snapshots cannot serve fresh checkouts.
/// No-op for static pools - overlay drift only applies to auth_query passthrough.
/// Used by the auth_query cache after a refetch when the new per-user
/// `startup_parameters` map no longer matches the snapshot frozen in
/// the live pool: the next client connection rebuilds the dynamic pool
/// against the new overlay.
pub fn drop_dynamic_pool(id: &PoolIdentifier) -> bool {
    // hold RELOAD_LOCK so the POOLS + DYNAMIC_POOLS updates are
    // a single cross-store transition; without it observers (or a
    // racing from_config) could see DYNAMIC_POOLS membership that
    // disagrees with POOLS, or the removal could be silently undone
    // by a concurrent register/GC/from_config that loaded an older
    // snapshot.
    let removed_pool = {
        let _guard = pool_write_lock();
        let dynamics = DYNAMIC_POOLS.load();
        if !dynamics.contains(id) {
            return false;
        }

        let pools = POOLS.load();
        let mut new_pools = (**pools).clone();
        let removed_pool = new_pools.remove(id);
        if removed_pool.is_some() {
            if let Some(pool) = &removed_pool {
                pool.database.close_new_checkouts();
            }
            POOLS.store(Arc::new(new_pools));
        }
        let mut new_set = (**dynamics).clone();
        new_set.remove(id);
        DYNAMIC_POOLS.store(Arc::new(new_set));
        removed_pool
    };
    if let Some(pool) = &removed_pool {
        pool.database.close();
    }
    removed_pool.is_some()
}

/// Drop a dynamic pool only if the live entry is still the pool generation
/// owned by a `PoolInitGuard`.
pub(crate) fn drop_dynamic_pool_if_init_guard_matches(
    id: &PoolIdentifier,
    init_complete: &Arc<AtomicBool>,
) -> bool {
    let removed_pool = {
        let _guard = pool_write_lock();
        let dynamics = DYNAMIC_POOLS.load();
        if !dynamics.contains(id) {
            return false;
        }

        let pools = POOLS.load();
        let Some(pool) = pools.get(id) else {
            return false;
        };
        if !Arc::ptr_eq(&pool.init_complete, init_complete) {
            return false;
        }

        let mut new_pools = (**pools).clone();
        let removed_pool = new_pools.remove(id);
        if let Some(pool) = &removed_pool {
            pool.database.close_new_checkouts();
            POOLS.store(Arc::new(new_pools));
        }

        let mut new_set = (**dynamics).clone();
        new_set.remove(id);
        DYNAMIC_POOLS.store(Arc::new(new_set));
        removed_pool
    };
    if let Some(pool) = &removed_pool {
        pool.database.close();
    }
    removed_pool.is_some()
}

/// Get auth_query state for a database pool.
pub fn get_auth_query_state(db: &str) -> Option<Arc<AuthQueryState>> {
    AUTH_QUERY_STATE.load().get(db).cloned()
}

/// An identifier for a PgDoorman pool.
#[derive(Hash, Debug, Clone, PartialEq, Eq, Default)]
pub struct PoolIdentifier {
    // The name of the database clients want to connect to.
    pub db: String,

    // The username the client connects with. Each user gets its own pool.
    pub user: String,
}

impl PoolIdentifier {
    /// Create a new user/pool identifier.
    pub fn new(db: &str, user: &str) -> PoolIdentifier {
        PoolIdentifier {
            db: db.to_string(),
            user: user.to_string(),
        }
    }
}

impl Display for PoolIdentifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.user, self.db)
    }
}

impl From<&Address> for PoolIdentifier {
    fn from(address: &Address) -> PoolIdentifier {
        PoolIdentifier::new(&address.database, &address.username)
    }
}

/// Pool settings.
#[derive(Clone, Debug)]
pub struct PoolSettings {
    /// Transaction or Session.
    pub pool_mode: PoolMode,

    // Connecting user.
    pub user: User,
    pub db: String,

    /// Синхронизируем серверные параметры установленные клиентом через SET. (False).
    pub sync_server_parameters: bool,

    /// Whether standalone DISCARD ALL may be acknowledged without a backend.
    pub intercept_discard_all: bool,

    idle_timeout_ms: u64,
    life_time_ms: u64,

    /// Pool-level minimum connections protected from coordinator eviction.
    /// Effective protection = max(user.min_pool_size, this value).
    pub min_guaranteed_pool_size: u32,
}

impl Default for PoolSettings {
    fn default() -> PoolSettings {
        PoolSettings {
            pool_mode: PoolMode::Transaction,
            user: User::default(),
            db: String::default(),
            idle_timeout_ms: General::default_idle_timeout().as_millis(),
            life_time_ms: General::default_server_lifetime().as_millis(),
            sync_server_parameters: General::default_sync_server_parameters(),
            intercept_discard_all: ConfigPool::default_intercept_discard_all(),
            min_guaranteed_pool_size: 0,
        }
    }
}

/// The globally accessible connection pool.
#[derive(Clone, Debug)]
pub struct ConnectionPool {
    /// The pool.
    pub database: Pool,

    /// The address (host, port)
    pub address: Address,

    /// The server information has to be passed to the
    /// clients on startup.
    original_server_parameters: ServerParametersType,

    /// Pool configuration.
    pub settings: PoolSettings,

    /// Hash value for the pool configs. It is used to compare new configs
    /// against current config to decide whether or not we need to recreate
    /// the pool after a RELOAD command
    pub config_hash: u64,

    /// Hash of the per-user auth_query overlay frozen into this pool at
    /// creation time. After a refetch, the auth_query cache compares the
    /// new per-user startup_parameters map against this value; a mismatch
    /// drops the dynamic pool so the next client connection rebuilds
    /// against the new overlay. Static pools and dedicated-mode shared
    /// pools both pin this to the empty-map hash.
    pub per_user_startup_overlay_hash: u64,

    /// Cache
    pub prepared_statement_cache: Option<PreparedStatementCacheType>,

    /// Per-pool cache for the response to `general.pooler_check_query`.
    /// Populated on the first matching SimpleQuery from any client; subsequent
    /// matches answer from this cache without touching the backend. The cache
    /// self-invalidates when `general.pooler_check_query` changes via RELOAD.
    pub check_query_cache: Arc<CheckQueryCache>,

    /// Database-level connection coordinator. `Some` when `max_db_connections > 0`
    /// in the pool config, `None` otherwise (disabled, zero overhead).
    /// Shared across all user pools for the same database.
    pub(crate) coordinator: Option<Arc<pool_coordinator::PoolCoordinator>>,

    /// Consecutive replenish failure counter for log noise suppression.
    /// Reset to 0 on successful replenish.
    pub(crate) replenish_failures: Arc<AtomicU32>,

    /// Whether the pool has completed initialization (first server
    /// connection established). Static pools start with `true`. Dynamic
    /// pools start with `false` and are flipped to `true` by
    /// `PoolInitGuard::commit` once `get_server_parameters` succeeds.
    /// GC skips pools with `init_complete == false` so a pool that is
    /// still establishing its first connection cannot be reaped while
    /// `pool_state().size` is still zero.
    pub(crate) init_complete: Arc<AtomicBool>,
}

impl ConnectionPool {
    #[cfg(test)]
    pub(crate) fn test_for_protocol() -> Self {
        let server_pool = ServerPool::new(
            Address::default(),
            User::default(),
            "test_db",
            Arc::new(DashMap::new()),
            false,
            false,
            0,
            "test_app".to_string(),
            1,
            60_000,
            60_000,
            60_000,
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
            false,
            None,
            Arc::new(std::collections::BTreeMap::new()),
            Arc::new(std::collections::BTreeMap::new()),
        );
        let database = Pool::builder(server_pool)
            .pool_name("test_db".to_string())
            .username("test_user".to_string())
            .build();
        ConnectionPool {
            database,
            address: Address::default(),
            original_server_parameters: Arc::new(tokio::sync::OnceCell::new()),
            settings: PoolSettings::default(),
            config_hash: 0,
            per_user_startup_overlay_hash: 0,
            prepared_statement_cache: Some(Arc::new(PreparedStatementCache::new(16, 1))),
            check_query_cache: Arc::new(CheckQueryCache::new()),
            coordinator: None,
            replenish_failures: Arc::new(AtomicU32::new(0)),
            init_complete: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Construct the connection pool from the configuration.
    pub async fn from_config(client_server_map: ClientServerMap) -> Result<(), Error> {
        // serialise the entire reload across the async
        // build phase. Without this, two concurrent SIGHUPs / admin
        // RELOADs both load the same POOLS baseline, both build, and
        // the second commit overwrites the first. RELOAD is rare so
        // serialising is cheap; we hold a `tokio::sync::Mutex` so
        // peer reloads await rather than racing.
        let _reload_guard = RELOAD_BUILD_LOCK.lock().await;
        let config = get_config();
        Self::from_config_locked(client_server_map, config, false).await
    }

    /// Construct pools from an explicit config snapshot without reading
    /// the global `CONFIG`. Reload uses this before publishing the parsed
    /// config, so a failed runtime apply cannot leak a half-applied config
    /// through admin/API readers.
    pub(crate) async fn from_config_snapshot(
        client_server_map: ClientServerMap,
        config: Config,
    ) -> Result<(), Error> {
        let _reload_guard = RELOAD_BUILD_LOCK.lock().await;
        Self::from_config_locked(client_server_map, config, true).await
    }

    async fn from_config_locked(
        client_server_map: ClientServerMap,
        config: Config,
        publish_config_before_pools: bool,
    ) -> Result<(), Error> {
        set_client_server_map(client_server_map.clone());
        let mut new_pools = HashMap::new();

        // Build per-database coordinators for pools with max_db_connections > 0.
        // Reuse existing coordinators when config hasn't changed (avoids resetting
        // semaphore state and losing in-flight permits on benign RELOAD).
        let mut coordinators: HashMap<String, Arc<pool_coordinator::PoolCoordinator>> =
            HashMap::new();
        let old_coordinators = COORDINATORS.load();
        for (pool_name, pool_config) in &config.pools {
            let max = pool_config.max_db_connections.unwrap_or(0) as usize;
            if max == 0 {
                continue;
            }
            let new_cfg = pool_coordinator::CoordinatorConfig {
                max_db_connections: max,
                min_connection_lifetime_ms: pool_config.min_connection_lifetime.unwrap_or(30_000),
                reserve_pool_size: pool_config.reserve_pool_size.unwrap_or(0) as usize,
                reserve_pool_timeout_ms: pool_config.reserve_pool_timeout.unwrap_or(3000),
            };
            // Reuse if config unchanged — keeps semaphores, arbiter, and in-flight permits alive.
            if let Some(existing) = old_coordinators.get(pool_name.as_str()) {
                if *existing.config() == new_cfg {
                    debug!("[pool: {pool_name}] coordinator config unchanged, reusing");
                    coordinators.insert(pool_name.clone(), existing.clone());
                    continue;
                }
                info!(
                    "[pool: {pool_name}] coordinator config changed, creating new (old connections drain naturally)"
                );
            } else {
                info!("[pool: {pool_name}] creating coordinator (max_db_connections={max})");
            }
            coordinators.insert(
                pool_name.clone(),
                pool_coordinator::PoolCoordinator::new(pool_name.clone(), new_cfg),
            );
        }

        // Hashing each pool's effective config against (Pool, general
        // startup_parameters baseline + sync_server_parameters) folds
        // general-level GUC changes into the same reuse decision pg_doorman
        // already uses for pool-level changes. Without this, a SIGHUP that
        // only edits `general.startup_parameters` or
        // `general.sync_server_parameters` would leave every idle backend
        // pinned to the previous `reset_val` until the connection rotates
        // through `lifetime_ms`, so clients would see mixed defaults from
        // the same pool depending on which backend they got.
        let general_startup_hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            config.general.startup_parameters.hash(&mut hasher);
            config.general.sync_server_parameters.hash(&mut hasher);
            hasher.finish()
        };
        // Load only; the hash is not advanced until the new pool map has
        // been committed at the bottom of from_config. Otherwise a reload
        // that fails halfway poisons the hash, and the next reload of the
        // *same* config silently skips the recycle of dynamic pools that
        // still carry the old reset_val.
        let previous_general_startup_hash = PREVIOUS_GENERAL_STARTUP_HASH.load(Ordering::Relaxed);
        // The static defaults to `0`, which collides with the empty-map
        // hash on a fresh process; treat that special case as "no prior
        // value" so the first reload never falsely claims a change.
        let general_startup_parameters_changed = previous_general_startup_hash != 0
            && previous_general_startup_hash != general_startup_hash;
        for (pool_name, pool_config) in &config.pools {
            let new_pool_hash_value = {
                use std::hash::Hasher;
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                hasher.write_u64(pool_config.hash_value());
                hasher.write_u64(general_startup_hash);
                hasher.finish()
            };
            let server_tls_config = build_server_tls_for_pool(pool_config, &config.general)?;

            // There is one pool per database/user pair.
            for user in &pool_config.users {
                let old_pool_ref = get_pool(pool_name, &user.username);
                let identifier = PoolIdentifier::new(pool_name, &user.username);

                if let Some(pool) = old_pool_ref {
                    // If the pool hasn't changed, get existing reference and insert it into the new_pools.
                    // We replace all pools at the end, but if the reference is kept, the pool won't get re-created (bb8).
                    if pool.config_hash == new_pool_hash_value
                        && pool.address.server_tls.as_ref() == server_tls_config.as_ref()
                    {
                        info!("[{}@{}] config unchanged", user.username, pool_name);
                        new_pools.insert(identifier.clone(), pool.clone());
                        continue;
                    }
                    if pool.config_hash == new_pool_hash_value
                        && pool.address.server_tls.as_ref() != server_tls_config.as_ref()
                    {
                        info!(
                            "[{}@{}] tls certificates changed on disk, recreating pool",
                            user.username, pool_name
                        );
                    }
                }

                info!("[{}@{}] creating pool", user.username, pool_name);

                // real database name on postgresql server.
                let server_database = pool_config
                    .server_database
                    .clone()
                    .unwrap_or(pool_name.clone().to_string());

                // Detect passthrough-eligible static users:
                // server_password is None AND (server_username is None OR equals username)
                let backend_auth = if user.server_password.is_none()
                    && (user.server_username.is_none()
                        || user.server_username.as_deref() == Some(&user.username))
                {
                    if user
                        .password
                        .starts_with(crate::messages::constants::MD5_PASSWORD_PREFIX)
                    {
                        info!(
                            "[{}@{}] static passthrough: MD5 pass-the-hash",
                            user.username, pool_name
                        );
                        Some(Arc::new(RwLock::new(BackendAuthMethod::Md5PassTheHash(
                            user.password.clone(),
                        ))))
                    } else if user
                        .password
                        .starts_with(crate::messages::constants::SCRAM_SHA_256)
                    {
                        info!(
                            "[{}@{}] static passthrough: SCRAM pending",
                            user.username, pool_name
                        );
                        Some(Arc::new(RwLock::new(BackendAuthMethod::ScramPending)))
                    } else {
                        None
                    }
                } else {
                    None
                };

                let address = Address {
                    database: pool_name.clone(),
                    host: pool_config.server_host.clone(),
                    port: pool_config.server_port,
                    username: user.username.clone(),
                    password: user.password.clone(),
                    pool_name: pool_name.clone(),
                    stats: Arc::new(AddressStats::default()),
                    backend_auth,
                    server_tls: server_tls_config.clone(),
                };

                let prepared_statements_cache_size = match config.general.prepared_statements {
                    true => pool_config
                        .prepared_statements_cache_size
                        .unwrap_or(config.general.prepared_statements_cache_size),
                    false => 0,
                };

                let server_prepared_statements_cache_size = resolve_server_cache_size(
                    prepared_statements_cache_size,
                    pool_config.server_prepared_statements_cache_size,
                    config.general.server_prepared_statements_cache_size,
                );

                let application_name = pool_config
                    .application_name
                    .clone()
                    .unwrap_or_else(|| "pg_doorman".to_string());

                let pool_mode = user.pool_mode.unwrap_or(pool_config.pool_mode);

                let fallback_state = build_fallback_state(pool_name, pool_config, &config.general);

                // Merge general+pool startup_parameters from the same
                // `config` snapshot we hashed above. ServerPool keeps this
                // as Arc<BTreeMap> for the rest of its life — the reload
                // path rebuilds the pool whenever either layer's hash
                // changes, so the snapshot stays valid until then. Passing
                // it in explicitly (rather than letting ServerPool::new
                // call config_arc() again) closes a narrow race where a
                // second reload between this iteration and constructor
                // execution would write a different baseline to the pool
                // than the one the reuse hash captured.
                let base_startup_parameters = Arc::new(
                    crate::config::startup_parameters::cascade_canonical_keys(&[
                        &config.general.startup_parameters,
                        &pool_config.startup_parameters,
                    ]),
                );

                // Per-user `prewarm_query` overrides the pool-level value when
                // present (even `Some(String::new())` - explicit disable for
                // one user). When absent, the pool default applies.
                let effective_prewarm = user
                    .prewarm_query
                    .clone()
                    .unwrap_or_else(|| pool_config.prewarm_query.clone());
                let connect_timeout = resolve_pool_connect_timeout(pool_config, &config.general);

                let manager = ServerPool::new(
                    address.clone(),
                    user.clone(),
                    server_database.as_str(),
                    client_server_map.clone(),
                    pool_config.cleanup_server_connections,
                    pool_config.log_client_parameter_status_changes,
                    server_prepared_statements_cache_size,
                    application_name,
                    config.general.max_concurrent_creates,
                    // prefer per-user `server_lifetime` over the pool
                    // default. Previously the User field was deserialised
                    // and round-tripped through annotated config writers
                    // but never read - operators who set
                    // `users.X.server_lifetime` silently got the pool
                    // default. Wire it through with the standard
                    // user.opt -> pool.opt -> general fallback chain.
                    user.server_lifetime
                        .or(pool_config.server_lifetime)
                        .unwrap_or(config.general.server_lifetime.as_millis()),
                    pool_config
                        .idle_timeout
                        .unwrap_or(config.general.idle_timeout.as_millis()),
                    config.general.server_idle_check_timeout.as_millis(),
                    connect_timeout,
                    config.general.query_wait_timeout.as_std(),
                    pool_mode == PoolMode::Session,
                    fallback_state,
                    base_startup_parameters,
                    // Static pools carry no per-user auth_query overlay.
                    Arc::new(std::collections::BTreeMap::new()),
                )
                .with_release_query(pool_config.release_query.clone())
                .with_prewarm_query(effective_prewarm)
                .with_intercept_discard_all(pool_config.intercept_discard_all);

                let queue_strategy = match config.general.server_round_robin {
                    true => QueueMode::Fifo,
                    false => QueueMode::Lifo,
                };

                let mut builder_config = Pool::builder(manager)
                    .coordinator(coordinators.get(pool_name).cloned())
                    .pool_name(pool_name.clone())
                    .username(user.username.clone());
                builder_config = builder_config.config(PoolConfig {
                    max_size: user.pool_size as usize,
                    timeouts: resolve_pool_timeouts(pool_config, &config.general),
                    queue_mode: queue_strategy,
                    scaling: pool_config.resolve_scaling_config(&config.general),
                });

                let pool = builder_config.build();

                let pool = ConnectionPool {
                    database: pool,
                    address,
                    config_hash: new_pool_hash_value,
                    // Static and dedicated-mode shared pools carry no
                    // per-user overlay, so they pin to the empty-map
                    // hash. Dynamic passthrough pools set this from the
                    // captured overlay in dynamic.rs.
                    per_user_startup_overlay_hash: empty_overlay_hash(),
                    original_server_parameters: Arc::new(tokio::sync::OnceCell::new()),
                    settings: PoolSettings {
                        pool_mode,
                        user: user.clone(),
                        db: pool_name.clone(),
                        idle_timeout_ms: pool_config
                            .idle_timeout
                            .unwrap_or(config.general.idle_timeout.as_millis()),
                        // per-user override of server_lifetime, same
                        // fallback chain as the ServerPool::new arg above.
                        life_time_ms: user
                            .server_lifetime
                            .or(pool_config.server_lifetime)
                            .unwrap_or(config.general.server_lifetime.as_millis()),
                        sync_server_parameters: pool_config.effective_sync_server_parameters(&config.general),
                        intercept_discard_all: pool_config.intercept_discard_all,
                        min_guaranteed_pool_size: pool_config.min_guaranteed_pool_size.unwrap_or(0),
                    },
                    prepared_statement_cache: match config.general.prepared_statements {
                        false => None,
                        true => Some(Arc::new(PreparedStatementCache::new(
                            prepared_statements_cache_size,
                            config.general.worker_threads,
                        ))),
                    },
                    check_query_cache: Arc::new(CheckQueryCache::new()),
                    coordinator: coordinators.get(pool_name).cloned(),
                    replenish_failures: Arc::new(AtomicU32::new(0)),
                    init_complete: Arc::new(AtomicBool::new(true)),
                };

                // There is one pool per database/user pair.
                new_pools.insert(PoolIdentifier::new(pool_name, &user.username), pool);
            }
        }

        // -----------------------------------------------------------------
        // Auth query: create AuthQueryState per pool (lazy executor init)
        // and shared connection pool for server_user mode.
        // -----------------------------------------------------------------
        let mut auth_query_states: HashMap<String, Arc<AuthQueryState>> = HashMap::new();

        let old_aq_states_for_reuse = AUTH_QUERY_STATE.load();

        for (pool_name, pool_config) in &config.pools {
            if let Some(ref aq_config) = pool_config.auth_query {
                let pool_startup_hash = {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    pool_config.startup_parameters.hash(&mut hasher);
                    hasher.finish()
                };
                // Parent fingerprint folds every other parent input the
                // dedicated shared pool depends on into one hash:
                // `pool_config.hash_value()` covers host/port/TLS/
                // timeouts/fallback/app_name/users; the general
                // startup hash covers the operator-wide baseline that
                // also flows into the shared pool's reset_val. A SIGHUP
                // that changes any of these without touching the
                // auth_query config still rebuilds the shared pool.
                let parent_fingerprint = pool_config.hash_value() ^ general_startup_hash;
                // RELOAD: reuse state when the auth_query config AND
                // the pool-level startup_parameters AND the parent
                // fingerprint are unchanged. Any other parent edit
                // (host/port/TLS/timeouts/fallback/app_name change,
                // general.startup_parameters edit) must drop the cache
                // and recycle the shared/dynamic pools: their backends
                // were started with the old parent inputs as
                // `reset_val` and TLS identity, and those survive
                // client-side `RESET ALL` / `DISCARD ALL` unless the
                // backend is recreated.
                if let Some(old_state) = old_aq_states_for_reuse.get(pool_name) {
                    if old_state.config == *aq_config
                        && old_state.pool_startup_hash == pool_startup_hash
                        && old_state.parent_fingerprint == parent_fingerprint
                    {
                        info!("[pool: {pool_name}] auth_query config unchanged — reusing state");
                        auth_query_states.insert(pool_name.clone(), old_state.clone());
                        // Still need to ensure shared pool exists in new_pools
                        if let Some(ref spid) = old_state.shared_pool_id {
                            if !new_pools.contains_key(spid) {
                                if let Some(pool) = POOLS.load().get(spid) {
                                    new_pools.insert(spid.clone(), pool.clone());
                                }
                            }
                        }
                        continue;
                    }
                }

                let shared_pool_id = if aq_config.is_dedicated_mode() {
                    let su = aq_config.server_user.as_ref().unwrap();
                    let identifier = PoolIdentifier::new(pool_name, su);

                    // Create the shared data pool if it doesn't already exist
                    // (a static user with the same name takes priority).
                    if !new_pools.contains_key(&identifier) {
                        let sp = aq_config
                            .server_password
                            .as_ref()
                            .cloned()
                            .unwrap_or_default();

                        let shared_user = User {
                            username: su.clone(),
                            password: String::new(),
                            pool_size: aq_config.pool_size,
                            server_username: Some(su.clone()),
                            server_password: Some(sp),
                            ..Default::default()
                        };

                        let server_database = pool_config
                            .server_database
                            .clone()
                            .unwrap_or_else(|| pool_name.to_string());
                        let server_tls_config =
                            build_server_tls_for_pool(pool_config, &config.general)?;

                        let address = Address {
                            database: pool_name.clone(),
                            host: pool_config.server_host.clone(),
                            port: pool_config.server_port,
                            username: shared_user.username.clone(),
                            password: shared_user.password.clone(),
                            pool_name: pool_name.clone(),
                            stats: Arc::new(AddressStats::default()),
                            backend_auth: None,
                            server_tls: server_tls_config,
                        };

                        let prepared_statements_cache_size =
                            match config.general.prepared_statements {
                                true => pool_config
                                    .prepared_statements_cache_size
                                    .unwrap_or(config.general.prepared_statements_cache_size),
                                false => 0,
                            };

                        let server_prepared_statements_cache_size = resolve_server_cache_size(
                            prepared_statements_cache_size,
                            pool_config.server_prepared_statements_cache_size,
                            config.general.server_prepared_statements_cache_size,
                        );

                        let application_name = pool_config
                            .application_name
                            .clone()
                            .unwrap_or_else(|| "pg_doorman".to_string());

                        let pool_mode = shared_user.pool_mode.unwrap_or(pool_config.pool_mode);

                        let fallback_state =
                            build_fallback_state(pool_name, pool_config, &config.general);

                        let base_startup_parameters = Arc::new(
                            crate::config::startup_parameters::cascade_canonical_keys(&[
                                &config.general.startup_parameters,
                                &pool_config.startup_parameters,
                            ]),
                        );

                        // Dedicated-mode shared pool uses pool-level prewarm
                        // only - there is no single per-user override that
                        // applies to all dynamic users behind one shared
                        // backend identity.
                        let effective_prewarm = pool_config.prewarm_query.clone();
                        let connect_timeout =
                            resolve_pool_connect_timeout(pool_config, &config.general);

                        let manager = ServerPool::new(
                            address.clone(),
                            shared_user.clone(),
                            server_database.as_str(),
                            client_server_map.clone(),
                            pool_config.cleanup_server_connections,
                            pool_config.log_client_parameter_status_changes,
                            server_prepared_statements_cache_size,
                            application_name,
                            config.general.max_concurrent_creates,
                            // dynamic auth_query pool uses a synthetic
                            // shared_user so per-user server_lifetime
                            // overrides don't apply - fall back to pool
                            // and general level. Documented to prevent a
                            // future refactor from silently dropping the
                            // per-user override here.
                            pool_config
                                .server_lifetime
                                .unwrap_or(config.general.server_lifetime.as_millis()),
                            pool_config
                                .idle_timeout
                                .unwrap_or(config.general.idle_timeout.as_millis()),
                            config.general.server_idle_check_timeout.as_millis(),
                            connect_timeout,
                            config.general.query_wait_timeout.as_std(),
                            pool_mode == PoolMode::Session,
                            fallback_state,
                            base_startup_parameters,
                            // Dedicated-mode shared pool serves multiple
                            // dynamic users — no single per-user override.
                            Arc::new(std::collections::BTreeMap::new()),
                        )
                        .with_release_query(pool_config.release_query.clone())
                        .with_prewarm_query(effective_prewarm)
                        .with_intercept_discard_all(pool_config.intercept_discard_all);

                        let queue_strategy = match config.general.server_round_robin {
                            true => QueueMode::Fifo,
                            false => QueueMode::Lifo,
                        };

                        info!(
                            "[{}@{}] creating auth_query shared pool",
                            shared_user.username, pool_name
                        );

                        let pool = Pool::builder(manager)
                            .coordinator(coordinators.get(pool_name).cloned())
                            .pool_name(pool_name.clone())
                            .username(shared_user.username.clone())
                            .config(PoolConfig {
                                max_size: shared_user.pool_size as usize,
                                timeouts: resolve_pool_timeouts(pool_config, &config.general),
                                queue_mode: queue_strategy,
                                scaling: pool_config.resolve_scaling_config(&config.general),
                            })
                            .build();

                        let new_pool_hash_value = {
                            use std::hash::Hasher;
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            hasher.write_u64(pool_config.hash_value());
                            hasher.write_u64(general_startup_hash);
                            hasher.finish()
                        };
                        let conn_pool = ConnectionPool {
                            database: pool,
                            address,
                            config_hash: new_pool_hash_value,
                            // Static and dedicated-mode shared pools carry no
                            // per-user overlay, so they pin to the empty-map
                            // hash. Dynamic passthrough pools set this from the
                            // captured overlay in dynamic.rs.
                            per_user_startup_overlay_hash: empty_overlay_hash(),
                            original_server_parameters: Arc::new(tokio::sync::OnceCell::new()),
                            settings: PoolSettings {
                                pool_mode,
                                user: shared_user,
                                db: pool_name.clone(),
                                idle_timeout_ms: pool_config
                                    .idle_timeout
                                    .unwrap_or(config.general.idle_timeout.as_millis()),
                                life_time_ms: pool_config
                                    .server_lifetime
                                    .unwrap_or(config.general.server_lifetime.as_millis()),
                                sync_server_parameters: pool_config.effective_sync_server_parameters(&config.general),
                                intercept_discard_all: pool_config.intercept_discard_all,
                                min_guaranteed_pool_size: pool_config
                                    .min_guaranteed_pool_size
                                    .unwrap_or(0),
                            },
                            prepared_statement_cache: match config.general.prepared_statements {
                                false => None,
                                true => Some(Arc::new(PreparedStatementCache::new(
                                    prepared_statements_cache_size,
                                    config.general.worker_threads,
                                ))),
                            },
                            check_query_cache: Arc::new(CheckQueryCache::new()),
                            coordinator: coordinators.get(pool_name).cloned(),
                            replenish_failures: Arc::new(AtomicU32::new(0)),
                            init_complete: Arc::new(AtomicBool::new(true)),
                        };

                        new_pools.insert(identifier.clone(), conn_pool);
                    }

                    Some(identifier)
                } else {
                    None // passthrough mode — dynamic pool created on first client connection
                };

                auth_query_states.insert(
                    pool_name.clone(),
                    Arc::new(AuthQueryState::new(
                        aq_config.clone(),
                        pool_startup_hash,
                        parent_fingerprint,
                        pool_name.clone(),
                        pool_config.server_host.clone(),
                        pool_config.server_port,
                        shared_pool_id,
                        Arc::new(AuthQueryStats::default()),
                    )),
                );
            }
        }

        let staged_runtime_dependencies = if publish_config_before_pools {
            Some(crate::config::stage_config_runtime_dependencies(&config)?)
        } else {
            None
        };
        let mut runtime_dependency_guards = if publish_config_before_pools {
            Some(crate::config::runtime_dependency_publish_guards().await)
        } else {
            None
        };

        // hold the write-serialisation lock across the final
        // dynamic-pool reconciliation as well as the ArcSwap publish.
        // Otherwise create_dynamic_pool() can insert into POOLS/
        // DYNAMIC_POOLS after this reload snapshots the old maps but
        // before it stores its prebuilt new_pools, returning a pool that
        // the reload immediately orphans.
        let _commit_guard = pool_write_lock();

        // --- RELOAD: detect auth_query config changes, manage dynamic pools ---
        let old_aq_states = old_aq_states_for_reuse;
        let mut pools_to_remove: Vec<PoolIdentifier> = Vec::new();

        // 1. Compare old vs new auth_query configs, plus pool-level
        //    startup_parameters: either change must drain dynamic pools
        //    for this pool_name so the next auth_query lookup builds
        //    fresh backends with the new baseline reset_val.
        for (pool_name, old_state) in old_aq_states.iter() {
            let new_pool_config = config.pools.get(pool_name);
            let new_aq = new_pool_config.and_then(|p| p.auth_query.as_ref());
            let aq_changed = match new_aq {
                None => true,                          // auth_query removed
                Some(new) => *new != old_state.config, // config changed
            };
            let new_pool_startup_hash = new_pool_config.map(|p| {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                p.startup_parameters.hash(&mut hasher);
                hasher.finish()
            });
            let new_parent_fingerprint =
                new_pool_config.map(|p| p.hash_value() ^ general_startup_hash);
            let pool_startup_changed = new_pool_startup_hash
                .map(|h| h != old_state.pool_startup_hash)
                .unwrap_or(false);
            let parent_fingerprint_changed = new_parent_fingerprint
                .map(|h| h != old_state.parent_fingerprint)
                .unwrap_or(false);
            if aq_changed || pool_startup_changed || parent_fingerprint_changed {
                let reason = if aq_changed {
                    "auth_query config changed"
                } else if pool_startup_changed {
                    "pool.startup_parameters changed"
                } else {
                    "parent pool/general config changed"
                };
                info!("[pool: {pool_name}] {reason} — collecting dynamic pools for removal");
                for id in DYNAMIC_POOLS.load().iter() {
                    if id.db == *pool_name {
                        pools_to_remove.push(id.clone());
                    }
                }
                old_state.try_clear_cache();
            }
        }

        // 2. Static user overrides dynamic pool
        for (pool_name, pool_config) in &config.pools {
            for user in &pool_config.users {
                let id = PoolIdentifier::new(pool_name, &user.username);
                if is_dynamic_pool(&id) && !pools_to_remove.contains(&id) {
                    info!(
                        "[pool: {pool_name}] static user '{}' overrides dynamic pool",
                        user.username
                    );
                    pools_to_remove.push(id);
                }
            }
        }

        // 2b. general.startup_parameters changed: drain every dynamic pool
        //     so the next auth_query lookup builds fresh backends with the
        //     new baseline. Static pools are already handled by the pool
        //     reuse hash above, which folds in `general_startup_hash`.
        if general_startup_parameters_changed {
            info!(
                "general.startup_parameters changed on reload — collecting all dynamic pools for recycle"
            );
            for id in DYNAMIC_POOLS.load().iter() {
                if !pools_to_remove.contains(id) {
                    pools_to_remove.push(id.clone());
                }
            }
        }

        // 3. Carry over surviving dynamic pools
        let old_pools = POOLS.load();
        let old_dynamic_pools = DYNAMIC_POOLS.load();
        let mut removed_dynamic_pools = Vec::new();
        for id in old_dynamic_pools.iter() {
            if pools_to_remove.contains(id) {
                continue;
            }
            if new_pools.contains_key(id) {
                continue;
            }
            if let Some(pool) = old_pools.get(id) {
                new_pools.insert(id.clone(), pool.clone());
            }
        }
        let mut removed_static_pools = Vec::new();
        for (id, pool) in old_pools.iter() {
            if old_dynamic_pools.contains(id) {
                continue;
            }
            let same_generation = new_pools
                .get(id)
                .map(|new_pool| Arc::ptr_eq(&pool.init_complete, &new_pool.init_complete))
                .unwrap_or(false);
            if !same_generation {
                pool.database.close_new_checkouts();
                removed_static_pools.push(pool.clone());
            }
        }

        // 4. Remove destroyed pools, update tracking + stats.
        //
        // Step 2 above adds an id to `pools_to_remove`
        // when a static user from the new config overrides a earlier-
        // dynamic pool - the intent is "the OLD dynamic pool entry is
        // gone, but the NEW static entry built by Step 1 (`pools_config`
        // construction) is the replacement". The legacy unconditional
        // `new_pools.remove(id)` here deleted the freshly-built static
        // entry too, so after the publish neither static (removed here)
        // nor dynamic (removed below at line ~1262) is reachable. The
        // next `get_pool()` returned None until a SECOND SIGHUP rebuilt
        // it; operators promoting a passthrough user to a static entry
        // saw silent auth outage. Skip `new_pools.remove` when the id is
        // present in the new config as a static (pool, user) pair -
        // DYNAMIC_POOLS still gets the demotion-to-static via the
        // existing loop below.
        for id in &pools_to_remove {
            let is_now_static = config
                .pools
                .get(&id.db)
                .map(|pool| pool.users.iter().any(|u| u.username == id.user))
                .unwrap_or(false);
            if let Some(pool) = old_pools.get(id) {
                pool.database.close_new_checkouts();
                removed_dynamic_pools.push(pool.clone());
            }
            if !is_now_static {
                new_pools.remove(id);
            }
            // Stats reflect the dynamic-pool destruction regardless: the
            // dynamic instance is gone (either replaced by a static one
            // or fully removed).
            if let Some(old_state) = old_aq_states.get(&id.db) {
                old_state
                    .stats
                    .dynamic_pools_destroyed
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        // Publish CONFIG after every fallible pool-build step has
        // succeeded, but before AUTH_QUERY_STATE becomes visible.
        // Otherwise a dynamic auth_query login can observe the new auth
        // state while create_dynamic_pool() still reads the old global
        // config.
        if publish_config_before_pools {
            crate::config::publish_config_snapshot(config.clone());
            crate::config::publish_staged_config_runtime_dependencies(
                staged_runtime_dependencies
                    .expect("staged runtime dependencies must exist for config reload"),
                runtime_dependency_guards
                    .as_mut()
                    .expect("runtime dependency guards must exist for config reload"),
            );
        }

        // hold the write-serialisation lock across the pool-state
        // ArcSwap publish. Other writers (register_dynamic_pool,
        // drop_dynamic_pool, gc, a second from_config) block until the
        // full reload is published.
        COORDINATORS.store(Arc::new(coordinators));
        AUTH_QUERY_STATE.store(Arc::new(auth_query_states));
        POOLS.store(Arc::new(new_pools.clone()));

        // for removals, publish POOLS first. Hot auth readers do
        // `get_pool()` then `is_dynamic_pool()`; deleting DYNAMIC_POOLS
        // first let them see the old dynamic pool in POOLS while
        // `is_dynamic_pool()` was already false.
        if !pools_to_remove.is_empty() {
            let mut new_dynamic = (**DYNAMIC_POOLS.load()).clone();
            for id in &pools_to_remove {
                new_dynamic.remove(id);
            }
            DYNAMIC_POOLS.store(Arc::new(new_dynamic));
            info!("RELOAD: removed {} dynamic pool(s)", pools_to_remove.len());
        }
        // Advance the recycle-watcher hash only after the new state is
        // published; a failure path above (Err returned via `?`) leaves
        // PREVIOUS_GENERAL_STARTUP_HASH alone so the next reload still
        // sees the old value and re-evaluates the change correctly.
        PREVIOUS_GENERAL_STARTUP_HASH.store(general_startup_hash, Ordering::Relaxed);
        drop(_commit_guard);
        for pool in removed_dynamic_pools {
            pool.database.close();
        }
        for pool in removed_static_pools {
            pool.database.close();
        }
        Ok(())
    }

    /// Get pool state for a particular shard server as reported by pooler.
    #[inline(always)]
    pub fn pool_state(&self) -> Status {
        self.database.status()
    }

    /// Get the address information for a server.
    #[inline(always)]
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Register a parse statement to the pool's cache and return the rewritten parse.
    ///
    /// `client_given_name` is the original Parse name from the client. `None`
    /// indicates an anonymous prepared statement (PostgreSQL's empty Parse
    /// name); `Some(name)` carries the client-supplied identifier. It is
    /// forwarded to the pool cache so each entry tracks whether it was ever
    /// Parse'd as a named statement, an anonymous one, or both — surfaced via
    /// `CacheEntryKind`.
    #[inline(always)]
    pub fn register_parse_to_cache(
        &self,
        hash: u64,
        parse: &Parse,
        client_given_name: Option<&str>,
        planner_param_hash: u64,
    ) -> Option<Arc<Parse>> {
        self.prepared_statement_cache.as_ref().map(|cache| {
            cache.get_or_insert_with_planner(parse, hash, client_given_name, planner_param_hash)
        })
    }

    /// Promote a prepared statement hash in the LRU
    #[inline(always)]
    pub fn promote_prepared_statement_hash(&self, hash: &u64) {
        if let Some(ref prepared_statement_cache) = self.prepared_statement_cache {
            prepared_statement_cache.promote(hash);
        }
    }

    pub async fn get_server_parameters(&mut self) -> Result<ServerParameters, Error> {
        // the prior shape was an async `Mutex` with a
        // double-check pattern. Under a reconnect storm the lock-held
        // section serialised through one tokio task, so login latency
        // fanned out linearly with concurrency on the steady-state hit
        // (Mutex acquire + clone). The new shape uses
        // `tokio::sync::OnceCell` which:
        //   1. Lock-free `get()` fast path on populated pools - readers
        //      never await each other.
        //   2. Single-shot async init for the very first caller; peers
        //      that arrive mid-init await the in-flight future via the
        //      OnceCell internals and then `clone` the populated value.
        //   3. PG-side rejection of an operator-supplied startup
        //      parameter keeps its typed shape (the `Err` is propagated
        //      out of the initialiser future so `get_or_try_init`
        //      surfaces it to every awaiting peer).
        if let Some(params) = self.original_server_parameters.get() {
            return Ok(params.clone());
        }
        let cell = Arc::clone(&self.original_server_parameters);
        let database = self.database.clone();
        let username = self.address.username.clone();
        let pool_name = self.address.pool_name.clone();
        let params = cell
            .get_or_try_init(|| async move {
                info!("[{username}@{pool_name}] fetching server parameters");
                let mut conn = match database.get().await {
                    Ok(conn) => conn,
                    // Forward the PG-side rejection verbatim so the
                    // cold auth path returns the original sqlstate +
                    // message instead of collapsing into 58000/3D000.
                    Err(PoolError::Backend(
                        err @ Error::ServerStartupParameterRejection { .. },
                    )) => return Err(err),
                    Err(err) => return Err(Error::ServerStartupReadParameters(err.to_string())),
                };
                let hashmap = conn.server_parameters_as_hashmap();
                let mut sp = ServerParameters::new();
                sp.set_from_hashmap(&hashmap, true);
                // Finish this cold-path internal checkout like a regular
                // one so the release-cleanup obligation armed at checkout
                // is discharged and the fresh backend returns to the pool
                // instead of being closed by the recycle-safety check.
                // Runs once per pool generation.
                conn.finalize_checkin().await?;
                Ok::<ServerParameters, Error>(sp)
            })
            .await?;
        Ok(params.clone())
    }

    /// Connections above the user's guaranteed minimum — these are eligible
    /// for eviction by the coordinator when another user needs a connection.
    /// Effective minimum = max(user.min_pool_size, pool.min_guaranteed_pool_size).
    pub fn spare_above_min(&self) -> usize {
        let current = self.pool_state().size;
        compute_spare(
            current,
            self.settings.user.min_pool_size,
            self.settings.min_guaranteed_pool_size,
        )
    }
}

/// Compute how many connections are above the effective guaranteed minimum.
/// Pure function extracted from `ConnectionPool::spare_above_min()` for testability.
fn compute_spare(
    current_pool_size: usize,
    user_min_pool_size: Option<u32>,
    pool_min_guaranteed: u32,
) -> usize {
    let user_min = user_min_pool_size.unwrap_or(0) as usize;
    let pool_min = pool_min_guaranteed as usize;
    let effective_min = user_min.max(pool_min);
    current_pool_size.saturating_sub(effective_min)
}

/// Build Patroni-assisted fallback state. Returns None when no `patroni_api_urls`
/// are configured at either pool or general level.
fn build_fallback_state(
    pool_name: &str,
    pool_config: &ConfigPool,
    general: &crate::config::General,
) -> Option<Arc<fallback::FallbackState>> {
    let urls = pool_config
        .patroni_api_urls
        .as_ref()
        .or(general.patroni_api_urls.as_ref())?;

    let cooldown = pool_config
        .fallback_cooldown
        .or(general.fallback_cooldown)
        .map(|d| d.as_std())
        .unwrap_or(std::time::Duration::from_secs(30));
    let api_timeout = pool_config
        .patroni_api_timeout
        .or(general.patroni_api_timeout)
        .map(|d| d.as_std())
        .unwrap_or(std::time::Duration::from_secs(5));
    let connect_timeout = pool_config
        .fallback_connect_timeout
        .or(general.fallback_connect_timeout)
        .map(|d| d.as_std())
        .unwrap_or(std::time::Duration::from_secs(5));
    let lifetime = pool_config
        .fallback_lifetime
        .or(general.fallback_lifetime)
        .map(|d| d.as_millis())
        .unwrap_or(cooldown.as_millis() as u64);

    match fallback::FallbackState::new(
        pool_name.to_string(),
        urls.clone(),
        cooldown,
        connect_timeout,
        api_timeout,
        lifetime,
    ) {
        Ok(state) => Some(Arc::new(state)),
        Err(e) => {
            log::error!("pool {pool_name}: Patroni-assisted fallback disabled: {e}");
            None
        }
    }
}

/// Resolve the per-backend prepared-statement LRU size for a pool.
///
/// Resolution order (most specific wins):
/// 1. `pool_override` (per-pool `server_prepared_statements_cache_size`)
/// 2. `general_override` (general-level `server_prepared_statements_cache_size`)
/// 3. fallback to `pool_cache_size` — the resolved
///    `prepared_statements_cache_size` for that pool, preserving the
///    behaviour from before this knob existed.
///
/// Returns 0 when `pool_cache_size` is 0: the pool-level cache is
/// disabled, so a per-backend LRU adds no value.
pub(crate) fn resolve_server_cache_size(
    pool_cache_size: usize,
    pool_override: Option<usize>,
    general_override: Option<usize>,
) -> usize {
    if pool_cache_size == 0 {
        return 0;
    }
    pool_override
        .or(general_override)
        .unwrap_or(pool_cache_size)
}

pub(crate) fn resolve_pool_connect_timeout(
    pool_config: &ConfigPool,
    general: &General,
) -> Duration {
    Duration::from_millis(
        pool_config
            .connect_timeout
            .unwrap_or(general.connect_timeout.as_millis()),
    )
}

pub(crate) fn resolve_pool_timeouts(pool_config: &ConfigPool, general: &General) -> Timeouts {
    Timeouts {
        wait: Some(general.query_wait_timeout.as_std()),
        create: Some(resolve_pool_connect_timeout(pool_config, general)),
        recycle: None,
    }
}

/// Pure helper that decides the per-client Anonymous LRU size from a
/// general config and an already-extracted per-pool override. Pulled
/// out so the unit tests can exercise the resolution table without
/// touching global pool state.
pub(crate) fn resolve_client_anon_cache_size_inner(
    general: &General,
    pool_override: Option<usize>,
) -> usize {
    if let Some(explicit) = general.client_anonymous_prepared_cache_size {
        return explicit;
    }
    pool_override.unwrap_or(general.prepared_statements_cache_size)
}

/// Resolve the per-client Anonymous LRU size for a connection coming
/// in on `pool_name`. Looks up the pool's
/// `prepared_statements_cache_size` override and feeds it into the
/// pure helper above. Falls back to general defaults when the pool
/// is not in static config — admin connections, dynamic auth_query
/// pools that have not been registered yet — matching pre-3.7
/// behaviour for those paths.
pub fn resolve_client_anon_cache_size(pool_name: &str, general: &General) -> usize {
    let pool_override = get_pool_config(pool_name).and_then(|p| p.prepared_statements_cache_size);
    resolve_client_anon_cache_size_inner(general, pool_override)
}

/// Get the connection pool
pub fn get_pool(db: &str, user: &str) -> Option<ConnectionPool> {
    (*(*POOLS.load()))
        .get(&PoolIdentifier::new(db, user))
        .cloned()
}

/// Returns true if the pool identified by `(db, user)` is registered.
///
/// Use this for routing checks that only need presence, not the pool clone.
pub fn pool_exists(db: &str, user: &str) -> bool {
    (*(*POOLS.load())).contains_key(&PoolIdentifier::new(db, user))
}

/// zero-allocation pool lookup for callers that
/// already hold a `PoolIdentifier` (typically cached once per client
/// in `Client.cached_pool_id`). The classic `get_pool(db, user)`
/// signature builds a fresh `PoolIdentifier` per call - two
/// `String::to_string()` on every checkout. On a 1k reconnects/sec
/// failover storm that is ~2k allocations/sec just to look up an
/// already-known pool key. This variant skips the allocation entirely.
#[inline]
pub fn get_pool_by_id(id: &PoolIdentifier) -> Option<ConnectionPool> {
    (*(*POOLS.load())).get(id).cloned()
}

/// Get pool-level configuration by database name.
/// Returns the Pool config if the database exists in configuration.
/// Used by auth_query to find auth_query config when user is not in static config.
pub fn get_pool_config(db: &str) -> Option<crate::config::Pool> {
    crate::config::config_arc().pools.get(db).cloned()
}

/// Get a pointer to all configured pools.
/// Returns an Arc to avoid cloning the entire HashMap on each call.
pub fn get_all_pools() -> Arc<PoolMap> {
    POOLS.load_full()
}

/// Get pool coordinator for a database pool (if `max_db_connections > 0`).
/// Returns `None` when coordination is disabled for this pool.
pub fn get_coordinator(db: &str) -> Option<Arc<pool_coordinator::PoolCoordinator>> {
    COORDINATORS.load().get(db).cloned()
}

// create_dynamic_pool is in dynamic.rs, re-exported above.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Pool as ConfigPool;

    #[test]
    fn pool_exists_returns_false_for_missing_entry() {
        // POOLS is global; use a pair no test should register.
        assert!(!pool_exists("nonexistent_db", "nonexistent_user"));
    }

    fn test_connection_pool_with_init_flag(init_complete: Arc<AtomicBool>) -> ConnectionPool {
        let server_pool = ServerPool::new(
            Address::default(),
            User::default(),
            "test_db",
            Arc::new(DashMap::new()),
            false,
            false,
            0,
            "test_app".to_string(),
            1,
            60_000,
            60_000,
            60_000,
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
            false,
            None,
            Arc::new(std::collections::BTreeMap::new()),
            Arc::new(std::collections::BTreeMap::new()),
        );
        let database = Pool::builder(server_pool)
            .pool_name("test_db".to_string())
            .username("test_user".to_string())
            .build();
        ConnectionPool {
            database,
            address: Address::default(),
            original_server_parameters: Arc::new(tokio::sync::OnceCell::new()),
            settings: PoolSettings {
                pool_mode: PoolMode::Transaction,
                user: User::default(),
                db: "test_db".to_string(),
                idle_timeout_ms: 60_000,
                life_time_ms: 60_000,
                sync_server_parameters: false,
                intercept_discard_all: true,
                min_guaranteed_pool_size: 0,
            },
            config_hash: 0,
            per_user_startup_overlay_hash: empty_overlay_hash(),
            prepared_statement_cache: None,
            check_query_cache: Arc::new(CheckQueryCache::new()),
            coordinator: None,
            replenish_failures: Arc::new(AtomicU32::new(0)),
            init_complete,
        }
    }

    // --- per_user_overlay_hash tests ---

    #[test]
    fn per_user_overlay_hash_empty_matches_empty_overlay_hash() {
        let empty_map = std::collections::HashMap::<String, String>::new();
        assert_eq!(
            per_user_overlay_hash(empty_map.iter()),
            empty_overlay_hash()
        );
    }

    #[test]
    fn per_user_overlay_hash_ignores_input_order() {
        // HashMap with the same key/value pairs but inserted in different
        // orders must hash identically. Without the internal sort the
        // hash would depend on HashMap iteration order, which is
        // randomized per process and would falsely flag overlay drift on
        // every refetch.
        let mut a = std::collections::HashMap::new();
        a.insert("work_mem".to_string(), "64MB".to_string());
        a.insert("statement_timeout".to_string(), "30s".to_string());
        let mut b = std::collections::HashMap::new();
        b.insert("statement_timeout".to_string(), "30s".to_string());
        b.insert("work_mem".to_string(), "64MB".to_string());
        assert_eq!(
            per_user_overlay_hash(a.iter()),
            per_user_overlay_hash(b.iter())
        );
    }

    #[test]
    fn per_user_overlay_hash_changes_when_value_changes() {
        let mut a = std::collections::HashMap::new();
        a.insert("work_mem".to_string(), "64MB".to_string());
        let mut b = std::collections::HashMap::new();
        b.insert("work_mem".to_string(), "128MB".to_string());
        assert_ne!(
            per_user_overlay_hash(a.iter()),
            per_user_overlay_hash(b.iter())
        );
    }

    #[test]
    fn per_user_overlay_hash_changes_when_key_added() {
        let mut a = std::collections::HashMap::new();
        a.insert("work_mem".to_string(), "64MB".to_string());
        let mut b = a.clone();
        b.insert("statement_timeout".to_string(), "30s".to_string());
        assert_ne!(
            per_user_overlay_hash(a.iter()),
            per_user_overlay_hash(b.iter())
        );
    }

    #[test]
    fn per_user_overlay_hash_matches_across_hashmap_and_btreemap() {
        // The auth_query cache stores HashMap; the pool freezes a
        // BTreeMap snapshot. Drift detection compares the two — they
        // must hash to the same value for identical content.
        let mut h = std::collections::HashMap::new();
        h.insert("work_mem".to_string(), "64MB".to_string());
        let mut b = std::collections::BTreeMap::new();
        b.insert("work_mem".to_string(), "64MB".to_string());
        assert_eq!(
            per_user_overlay_hash(h.iter()),
            per_user_overlay_hash(b.iter())
        );
    }

    // --- compute_spare tests ---

    #[test]
    fn spare_no_minimums_set() {
        // No min_pool_size, no min_guaranteed → all connections are spare
        assert_eq!(compute_spare(5, None, 0), 5);
    }

    #[test]
    fn spare_with_user_min_pool_size_only() {
        // user.min_pool_size=3, pool.min_guaranteed=0 → effective_min=3
        assert_eq!(compute_spare(5, Some(3), 0), 2);
    }

    #[test]
    fn spare_with_pool_guaranteed_only() {
        // user.min_pool_size=None, pool.min_guaranteed=4 → effective_min=4
        assert_eq!(compute_spare(5, None, 4), 1);
    }

    #[test]
    fn spare_pool_guaranteed_wins_over_user_min() {
        // user.min_pool_size=2, pool.min_guaranteed=4 → effective_min=max(2,4)=4
        assert_eq!(compute_spare(5, Some(2), 4), 1);
    }

    #[test]
    fn spare_user_min_wins_over_pool_guaranteed() {
        // user.min_pool_size=5, pool.min_guaranteed=2 → effective_min=max(5,2)=5
        assert_eq!(compute_spare(5, Some(5), 2), 0);
    }

    #[test]
    fn spare_at_exact_minimum() {
        // current == effective_min → 0 spare
        assert_eq!(compute_spare(3, Some(3), 0), 0);
        assert_eq!(compute_spare(4, None, 4), 0);
    }

    #[test]
    fn spare_below_minimum_saturates_to_zero() {
        // current < effective_min → saturating_sub returns 0
        assert_eq!(compute_spare(2, Some(5), 0), 0);
        assert_eq!(compute_spare(1, None, 3), 0);
        assert_eq!(compute_spare(0, Some(1), 2), 0);
    }

    #[test]
    fn spare_zero_current_connections() {
        assert_eq!(compute_spare(0, None, 0), 0);
        assert_eq!(compute_spare(0, Some(3), 5), 0);
    }

    #[test]
    fn spare_both_minimums_equal() {
        // user.min_pool_size=3, pool.min_guaranteed=3 → effective_min=3
        assert_eq!(compute_spare(5, Some(3), 3), 2);
    }

    #[test]
    fn spare_large_values() {
        assert_eq!(compute_spare(1000, Some(100), 200), 800);
        assert_eq!(compute_spare(1000, Some(999), 1), 1);
    }

    // --- resolve_server_cache_size tests ---

    #[test]
    fn server_cache_size_defaults_to_pool_size() {
        // Neither override is set → inherit pool_cache_size.
        assert_eq!(resolve_server_cache_size(8192, None, None), 8192);
    }

    #[test]
    fn server_cache_size_general_override_takes_effect() {
        // General override applied when pool override absent.
        assert_eq!(resolve_server_cache_size(8192, None, Some(1024)), 1024);
    }

    #[test]
    fn server_cache_size_pool_override_wins_over_general() {
        // Per-pool override is the most specific level.
        assert_eq!(
            resolve_server_cache_size(8192, Some(2048), Some(1024)),
            2048
        );
    }

    #[test]
    fn server_cache_size_pool_override_wins_over_inheritance() {
        assert_eq!(resolve_server_cache_size(8192, Some(2048), None), 2048);
    }

    #[test]
    fn server_cache_size_zero_pool_disables_server_lru() {
        // pool_cache_size=0 means caches are off; server LRU is forced to 0
        // regardless of overrides.
        assert_eq!(resolve_server_cache_size(0, None, None), 0);
        assert_eq!(resolve_server_cache_size(0, Some(1024), None), 0);
        assert_eq!(resolve_server_cache_size(0, None, Some(1024)), 0);
        assert_eq!(resolve_server_cache_size(0, Some(2048), Some(1024)), 0);
    }

    #[test]
    fn server_cache_size_explicit_zero_per_pool_allowed() {
        // Operators may explicitly disable the per-backend LRU even with a
        // positive pool cache; resolve must return 0 in that case.
        assert_eq!(resolve_server_cache_size(8192, Some(0), None), 0);
        assert_eq!(resolve_server_cache_size(8192, Some(0), Some(1024)), 0);
    }

    // --- resolve_client_anon_cache_size_inner tests ---

    #[test]
    fn anon_cache_inherits_general_when_no_overrides() {
        let g = General::test_with_cache_sizes(8192, None);
        assert_eq!(resolve_client_anon_cache_size_inner(&g, None), 8192);
    }

    #[test]
    fn anon_cache_uses_pool_override_when_no_explicit() {
        let g = General::test_with_cache_sizes(8192, None);
        assert_eq!(resolve_client_anon_cache_size_inner(&g, Some(1024)), 1024);
    }

    #[test]
    fn anon_cache_explicit_wins_over_pool_override() {
        let g = General::test_with_cache_sizes(8192, Some(256));
        assert_eq!(resolve_client_anon_cache_size_inner(&g, Some(1024)), 256);
        assert_eq!(resolve_client_anon_cache_size_inner(&g, None), 256);
    }

    #[test]
    fn anon_cache_explicit_zero_disables_lru_regardless_of_pool() {
        let g = General::test_with_cache_sizes(8192, Some(0));
        assert_eq!(resolve_client_anon_cache_size_inner(&g, Some(1024)), 0);
    }

    // --- sync_server_parameters reload tests ---

    #[test]
    fn effective_sync_server_parameters_uses_pool_override_when_set() {
        let general = General::default();
        let pool = ConfigPool {
            sync_server_parameters: Some(true),
            ..Default::default()
        };
        assert!(pool.effective_sync_server_parameters(&general));
    }

    #[test]
    fn effective_sync_server_parameters_falls_back_to_general_when_none() {
        let mut general = General::default();
        general.sync_server_parameters = true;
        let pool = ConfigPool {
            sync_server_parameters: None,
            ..Default::default()
        };
        assert!(pool.effective_sync_server_parameters(&general));
    }

    #[test]
    fn effective_sync_server_parameters_false_general_false_pool_none() {
        let general = General::default(); // sync_server_parameters defaults to false
        let pool = ConfigPool {
            sync_server_parameters: None,
            ..Default::default()
        };
        assert!(!pool.effective_sync_server_parameters(&general));
    }

    #[test]
    fn effective_sync_server_parameters_pool_override_wins_over_general() {
        let mut general = General::default();
        general.sync_server_parameters = true;
        let pool = ConfigPool {
            sync_server_parameters: Some(false),
            ..Default::default()
        };
        // Pool override (false) wins over general (true)
        assert!(!pool.effective_sync_server_parameters(&general));
    }

    fn compute_general_startup_hash(general: &General) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        general.startup_parameters.hash(&mut hasher);
        general.sync_server_parameters.hash(&mut hasher);
        hasher.finish()
    }

    fn compute_pool_fingerprint(pool: &ConfigPool, general: &General) -> u64 {
        use std::hash::Hasher;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hasher.write_u64(pool.hash_value());
        hasher.write_u64(compute_general_startup_hash(general));
        hasher.finish()
    }

    #[test]
    fn reload_sync_server_parameters_false_to_true_changes_general_startup_hash() {
        let mut general_before = General::default();
        general_before.sync_server_parameters = false;
        let mut general_after = General::default();
        general_after.sync_server_parameters = true;
        assert_ne!(
            compute_general_startup_hash(&general_before),
            compute_general_startup_hash(&general_after),
            "Changing general.sync_server_parameters must change the general_startup_hash"
        );
    }

    #[test]
    fn reload_sync_server_parameters_true_to_false_changes_general_startup_hash() {
        let mut general_before = General::default();
        general_before.sync_server_parameters = true;
        let mut general_after = General::default();
        general_after.sync_server_parameters = false;
        assert_ne!(
            compute_general_startup_hash(&general_before),
            compute_general_startup_hash(&general_after),
            "Changing general.sync_server_parameters must change the general_startup_hash"
        );
    }

    #[test]
    fn reload_general_sync_server_parameters_changes_static_pool_fingerprint() {
        let pool = ConfigPool::default();
        let mut general_before = General::default();
        general_before.sync_server_parameters = false;
        let mut general_after = General::default();
        general_after.sync_server_parameters = true;
        assert_ne!(
            compute_pool_fingerprint(&pool, &general_before),
            compute_pool_fingerprint(&pool, &general_after),
            "Changing general.sync_server_parameters must invalidate static pool fingerprint"
        );
    }

    #[test]
    fn reload_general_sync_server_parameters_changes_auth_query_parent_fingerprint() {
        let pool = ConfigPool::default();
        let mut general_before = General::default();
        general_before.sync_server_parameters = false;
        let mut general_after = General::default();
        general_after.sync_server_parameters = true;
        // parent_fingerprint = pool_config.hash_value() ^ general_startup_hash
        let fp_before = pool.hash_value() ^ compute_general_startup_hash(&general_before);
        let fp_after = pool.hash_value() ^ compute_general_startup_hash(&general_after);
        assert_ne!(
            fp_before, fp_after,
            "Changing general.sync_server_parameters must invalidate auth_query parent_fingerprint"
        );
    }

    #[test]
    fn reload_pool_sync_server_parameters_none_to_true_changes_fingerprint() {
        let mut general = General::default();
        general.sync_server_parameters = false;
        let pool_before = ConfigPool {
            sync_server_parameters: None,
            ..Default::default()
        };
        let pool_after = ConfigPool {
            sync_server_parameters: Some(true),
            ..Default::default()
        };
        assert_ne!(
            compute_pool_fingerprint(&pool_before, &general),
            compute_pool_fingerprint(&pool_after, &general),
            "Setting pool.sync_server_parameters must change fingerprint"
        );
    }

    #[test]
    fn reload_no_change_preserves_fingerprint() {
        let general = General::default();
        let pool = ConfigPool::default();
        assert_eq!(
            compute_pool_fingerprint(&pool, &general),
            compute_pool_fingerprint(&pool, &general),
            "Same config must produce identical fingerprint"
        );
    }

    #[test]
    fn pool_connect_timeout_override_feeds_connect_and_create_deadlines() {
        let general = General {
            connect_timeout: crate::config::Duration::from_millis(30_000),
            query_wait_timeout: crate::config::Duration::from_millis(10_000),
            ..General::default()
        };

        let pool_config = ConfigPool {
            connect_timeout: Some(250),
            ..ConfigPool::default()
        };

        assert_eq!(
            resolve_pool_connect_timeout(&pool_config, &general),
            Duration::from_millis(250)
        );
        assert_eq!(
            resolve_pool_timeouts(&pool_config, &general).create,
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            resolve_pool_timeouts(&pool_config, &general).wait,
            Some(Duration::from_millis(10_000))
        );
    }

    #[test]
    fn reload_unchanged_sync_server_parameters_preserves_fingerprint() {
        let mut general = General::default();
        general.sync_server_parameters = true;
        let pool = ConfigPool {
            sync_server_parameters: Some(true),
            ..Default::default()
        };
        // Pool-level override is set; general value is different but
        // pool override wins, so effective value is the same.
        let mut general2 = General::default();
        general2.sync_server_parameters = false;
        let pool2 = ConfigPool {
            sync_server_parameters: Some(true),
            ..Default::default()
        };
        // general changed, but pool override is the same → fingerprint
        // still changes because general_startup_hash is part of it.
        // This is expected: the fingerprint detects ANY general change,
        // even if the effective value is unchanged. This is a safe
        // over-rebuild rather than under-rebuild.
        assert_ne!(
            compute_pool_fingerprint(&pool, &general),
            compute_pool_fingerprint(&pool2, &general2),
        );
    }
    #[test]
    fn pool_connect_timeout_inherits_general_when_unset() {
        let general = General {
            connect_timeout: crate::config::Duration::from_millis(7500),
            ..General::default()
        };

        let pool_config = ConfigPool::default();

        assert_eq!(
            resolve_pool_connect_timeout(&pool_config, &general),
            Duration::from_millis(7500)
        );
        assert_eq!(
            resolve_pool_timeouts(&pool_config, &general).create,
            Some(Duration::from_millis(7500))
        );
    }

    // --- concurrent DYNAMIC_POOLS mutation under RELOAD_LOCK ---

    /// Race scenario: N threads call `register_dynamic_pool` for N
    /// distinct identifiers at the same time. Before that race each thread
    /// did `DYNAMIC_POOLS.load() -> clone -> insert -> store` without
    /// serialisation, so two writers that loaded the same snapshot
    /// would each store a clone with only their own insert and the
    /// loser's insert would be silently dropped (last-write-wins).
    /// With RELOAD_LOCK serialising the load+store sequence every
    /// insert must survive.
    ///
    /// The test uses a shared prefix unique to this test run so a
    /// concurrent test executing on the same process cannot leak
    /// identifiers into our assertion set.
    #[test]
    fn register_dynamic_pool_survives_concurrent_inserts() {
        use std::sync::atomic::{AtomicU64, Ordering as AOrd};
        use std::sync::Barrier;
        use std::thread;

        static RUN_ID: AtomicU64 = AtomicU64::new(0);
        let run = RUN_ID.fetch_add(1, AOrd::Relaxed);
        let prefix = format!("h16_concurrent_run_{run}_");

        // Snapshot membership before the test so we only assert on
        // identifiers this test created (other tests in the same
        // process may have registered their own).
        let before: HashSet<PoolIdentifier> = (**DYNAMIC_POOLS.load()).clone();

        const N: usize = 32;
        let barrier = Arc::new(Barrier::new(N));
        let prefix = Arc::new(prefix);
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let b = Arc::clone(&barrier);
            let p = Arc::clone(&prefix);
            handles.push(thread::spawn(move || {
                let id = PoolIdentifier::new(&format!("{p}db_{i}"), "u");
                b.wait();
                register_dynamic_pool(&id);
            }));
        }
        for h in handles {
            h.join().expect("register thread panicked");
        }

        let after = DYNAMIC_POOLS.load();
        let mut missing = Vec::new();
        for i in 0..N {
            let id = PoolIdentifier::new(&format!("{prefix}db_{i}"), "u");
            if !after.contains(&id) {
                missing.push(id);
            }
        }
        assert!(
            missing.is_empty(),
            "concurrent register_dynamic_pool lost inserts: {missing:?}"
        );

        // Cleanup so repeated test runs don't leak. drop_dynamic_pool
        // walks POOLS which we never populated - the pool entry is
        // absent and HashMap::remove is a no-op. We patch DYNAMIC_POOLS
        // directly to remove only our inserts; other tests' entries
        // (in `before`) are preserved.
        let _g = pool_write_lock();
        let mut current = (**DYNAMIC_POOLS.load()).clone();
        for i in 0..N {
            let id = PoolIdentifier::new(&format!("{prefix}db_{i}"), "u");
            current.remove(&id);
        }
        // Re-add any identifiers that were present before us but got
        // accidentally removed by a parallel test sharing our prefix
        // (defensive - prefix is unique per call so this should be a
        // no-op).
        for id in &before {
            if !current.contains(id) {
                current.insert(id.clone());
            }
        }
        DYNAMIC_POOLS.store(Arc::new(current));
    }

    #[test]
    fn reload_dynamic_removal_publishes_pools_before_membership_removal() {
        let src = include_str!("mod.rs");
        let start = src
            .find("// 4. Remove destroyed pools, update tracking + stats")
            .expect("reload removal block marker must exist");
        let end = src[start..]
            .find("PREVIOUS_GENERAL_STARTUP_HASH.store")
            .map(|offset| start + offset)
            .expect("reload publish tail marker must exist");
        let block = &src[start..end];

        let pools_store = block
            .find("POOLS.store")
            .expect("reload block must publish POOLS");
        let dynamic_store = block
            .find("DYNAMIC_POOLS.store")
            .expect("reload block must update DYNAMIC_POOLS");

        assert!(
            pools_store < dynamic_store,
            "dynamic removal must publish POOLS before DYNAMIC_POOLS so readers \
             cannot see old POOLS entries after is_dynamic_pool() becomes false"
        );
    }

    #[test]
    fn reload_publishes_config_before_auth_query_state() {
        let src = include_str!("mod.rs");
        let start = src
            .find("// Publish CONFIG after every fallible pool-build step")
            .expect("reload publish block marker must exist");
        let end = src[start..]
            .find("PREVIOUS_GENERAL_STARTUP_HASH.store")
            .map(|offset| start + offset)
            .expect("reload publish tail marker must exist");
        let block = &src[start..end];

        let config_publish = block
            .find("crate::config::publish_config")
            .expect("reload block must publish CONFIG");
        let auth_query_publish = block
            .find("AUTH_QUERY_STATE.store")
            .expect("reload block must publish AUTH_QUERY_STATE");

        assert!(
            config_publish < auth_query_publish,
            "auth_query readers must not see the new AUTH_QUERY_STATE while \
             create_dynamic_pool() still reads the old global config"
        );
    }

    #[test]
    fn dedicated_auth_query_shared_pool_hash_includes_general_startup() {
        let src = include_str!("mod.rs");
        let start = src
            .find("[{}@{}] creating auth_query shared pool")
            .expect("dedicated auth_query shared pool build block must exist");
        let block = &src[start..];
        let hash_start = block
            .find("let new_pool_hash_value =")
            .expect("dedicated auth_query shared pool hash must be computed");
        let hash_block = &block[hash_start..];
        let hash_end = hash_block
            .find("let conn_pool = ConnectionPool")
            .expect("connection pool creation should follow hash computation");
        let hash_block = &hash_block[..hash_end];

        assert!(
            hash_block.contains("pool_config.hash_value()"),
            "dedicated auth_query shared pool hash must include the pool config fingerprint"
        );
        assert!(
            hash_block.contains("general_startup_hash"),
            "dedicated auth_query shared pool hash must include general.startup_parameters \
             like static pools do, otherwise migration accepts stale startup baselines"
        );
    }

    #[test]
    fn reload_reconciles_dynamic_pools_under_commit_lock() {
        let src = include_str!("mod.rs");
        let start = src
            .find("// hold the write-serialisation lock")
            .expect("reload dynamic reconciliation marker must exist");
        let end = src[start..]
            .find("PREVIOUS_GENERAL_STARTUP_HASH.store")
            .map(|offset| start + offset)
            .expect("reload publish tail marker must exist");
        let block = &src[start..end];

        let commit_lock = block
            .find("let _commit_guard = pool_write_lock()")
            .expect("reload commit block must take the pool write lock");
        let dynamic_snapshot = block
            .find("DYNAMIC_POOLS.load")
            .expect("reload reconciliation must snapshot dynamic pools");
        let config_publish = block
            .find("crate::config::publish_config")
            .or_else(|| block.find("crate::config::publish_config_snapshot"))
            .expect("reload block must publish CONFIG");

        assert!(
            commit_lock < dynamic_snapshot,
            "reload must take pool_write_lock before snapshotting dynamic pools so \
             create_dynamic_pool() cannot publish a pool that the reload snapshot drops"
        );
        assert!(
            commit_lock < config_publish,
            "reload must hold pool_write_lock before CONFIG becomes visible so \
             auth_query dynamic writers cannot commit under the new config before POOLS"
        );
    }

    #[test]
    fn reload_stages_runtime_dependencies_until_config_publish() {
        let src = include_str!("mod.rs");
        let commit_start = src
            .find("// hold the write-serialisation lock")
            .expect("reload commit block marker must exist");
        let precommit = &src[..commit_start];

        assert!(
            !precommit.contains("publish_config_runtime_dependencies(&config).await"),
            "reload must not publish auth runtime dependencies before the commit window"
        );

        let start = src
            .find("// Publish CONFIG after every fallible pool-build step")
            .expect("reload config publish marker must exist");
        let end = src[start..]
            .find("PREVIOUS_GENERAL_STARTUP_HASH.store")
            .map(|offset| start + offset)
            .expect("reload publish tail marker must exist");
        let block = &src[start..end];

        let config_publish = block
            .find("crate::config::publish_config_snapshot")
            .expect("reload block must publish CONFIG");
        let runtime_publish = block
            .find("crate::config::publish_staged_config_runtime_dependencies")
            .expect("reload block must publish staged auth runtime dependencies");

        assert!(
            config_publish < runtime_publish,
            "auth runtime dependencies must become visible with the same reload generation as CONFIG"
        );
    }

    #[test]
    fn reload_closes_replaced_static_generations_before_publish() {
        let src = include_str!("mod.rs");
        let start = src
            .find("// 3. Carry over surviving dynamic pools")
            .expect("reload pool reconciliation marker must exist");
        let end = src[start..]
            .find("\n        Ok(())")
            .map(|offset| start + offset)
            .expect("reload publish tail marker must exist");
        let block = &src[start..end];

        let removed_static_decl = block
            .find("removed_static_pools")
            .expect("reload must collect removed/replaced static generations");
        let close_new_idx = block[removed_static_decl..]
            .find("close_new_checkouts()")
            .map(|offset| removed_static_decl + offset)
            .expect("removed/replaced static generations must close new checkouts");
        let pools_store_idx = block
            .find("POOLS.store")
            .expect("reload must publish POOLS");
        let drop_guard_idx = block
            .find("drop(_commit_guard)")
            .expect("reload must release pool_write_lock before draining old generations");
        let close_idx = block[drop_guard_idx..]
            .find("removed_static_pools")
            .map(|offset| drop_guard_idx + offset)
            .expect("reload must drain removed/replaced static generations after unlock");

        assert!(
            removed_static_decl < close_new_idx && close_new_idx < pools_store_idx,
            "reload must close new checkouts on removed/replaced static generations before POOLS publish"
        );
        assert!(
            pools_store_idx < drop_guard_idx && drop_guard_idx < close_idx,
            "reload must close removed/replaced static generations after publishing and releasing pool_write_lock"
        );
    }

    #[test]
    fn reload_closes_removed_dynamic_generations_before_publish() {
        let src = include_str!("mod.rs");
        let start = src
            .find("// 4. Remove destroyed pools, update tracking + stats.")
            .expect("reload removal block marker must exist");
        let end = src[start..]
            .find("\n        Ok(())")
            .map(|offset| start + offset)
            .expect("reload publish tail marker must exist");
        let block = &src[start..end];

        let removal_loop_idx = block
            .find("for id in &pools_to_remove")
            .expect("reload should loop over dynamic removals");
        let publish_idx = block
            .find("POOLS.store")
            .expect("reload should publish POOLS after dynamic removals");
        let dynamic_section = &block[removal_loop_idx..publish_idx];
        assert!(
            dynamic_section.contains("close_new_checkouts"),
            "removing a dynamic pool from global maps must close new checkouts \
             before publishing the replacement/removal"
        );

        let after_unlock = block
            .split("drop(_commit_guard)")
            .nth(1)
            .expect("reload should release commit guard before draining old static generations");
        assert!(
            after_unlock.contains("for pool in removed_dynamic_pools")
                && after_unlock.contains(".database.close()"),
            "reload must drain removed dynamic generations after publish so \
             retired auth_query snapshots cannot serve fresh checkouts"
        );
    }

    #[test]
    fn stale_init_guard_drop_does_not_remove_recreated_dynamic_pool() {
        let id = PoolIdentifier::new("init_guard_stale_db", "user");
        let stale_flag = Arc::new(AtomicBool::new(false));
        let live_flag = Arc::new(AtomicBool::new(false));
        let live_pool = test_connection_pool_with_init_flag(Arc::clone(&live_flag));

        {
            let _g = pool_write_lock();
            let mut pools = (**POOLS.load()).clone();
            pools.insert(id.clone(), live_pool);
            POOLS.store(Arc::new(pools));

            let mut dynamic = (**DYNAMIC_POOLS.load()).clone();
            dynamic.insert(id.clone());
            DYNAMIC_POOLS.store(Arc::new(dynamic));
        }

        drop(PoolInitGuard::for_new_pool(id.clone(), stale_flag));

        let pools = POOLS.load();
        let live = pools
            .get(&id)
            .expect("stale init guard must not remove a newer pool with the same identifier");
        assert!(
            Arc::ptr_eq(&live.init_complete, &live_flag),
            "stale init guard must only clean up the pool generation it owns"
        );
        assert!(
            DYNAMIC_POOLS.load().contains(&id),
            "stale init guard must not drop dynamic membership for the newer pool"
        );

        let _g = pool_write_lock();
        let mut pools = (**POOLS.load()).clone();
        pools.remove(&id);
        POOLS.store(Arc::new(pools));
        let mut dynamic = (**DYNAMIC_POOLS.load()).clone();
        dynamic.remove(&id);
        DYNAMIC_POOLS.store(Arc::new(dynamic));
    }

    #[test]
    fn drop_dynamic_pool_closes_removed_generation_for_revoked_dynamic_clients() {
        let id = PoolIdentifier::new("dynamic_drop_close_db", "user");
        let removed_pool = test_connection_pool_with_init_flag(Arc::new(AtomicBool::new(true)));
        let held_by_client = removed_pool.clone();

        {
            let _g = pool_write_lock();
            let mut pools = (**POOLS.load()).clone();
            pools.insert(id.clone(), removed_pool);
            POOLS.store(Arc::new(pools));

            let mut dynamic = (**DYNAMIC_POOLS.load()).clone();
            dynamic.insert(id.clone());
            DYNAMIC_POOLS.store(Arc::new(dynamic));
        }

        assert!(drop_dynamic_pool(&id));
        assert!(
            held_by_client.database.is_closed(),
            "dropping a dynamic pool must close the removed generation so \
             revoked or remapped auth_query snapshots cannot keep creating backends"
        );
        assert!(
            !POOLS.load().contains_key(&id),
            "removed dynamic pool must no longer be reachable by new global lookups"
        );
        assert!(
            !DYNAMIC_POOLS.load().contains(&id),
            "removed dynamic pool must no longer be advertised as dynamic"
        );

        let _g = pool_write_lock();
        let mut pools = (**POOLS.load()).clone();
        pools.remove(&id);
        POOLS.store(Arc::new(pools));
        let mut dynamic = (**DYNAMIC_POOLS.load()).clone();
        dynamic.remove(&id);
        DYNAMIC_POOLS.store(Arc::new(dynamic));
    }

    #[test]
    fn drop_dynamic_pool_revalidates_dynamic_membership_under_lock_before_remove() {
        let src = include_str!("mod.rs");
        let start = src
            .find("pub fn drop_dynamic_pool(id: &PoolIdentifier) -> bool")
            .expect("drop_dynamic_pool must exist");
        let body = &src[start..];
        let end = body
            .find("/// Drop a dynamic pool only if the live entry")
            .expect("drop_dynamic_pool end marker must exist");
        let body = &body[..end];

        let lock_pos = body
            .find("let _guard = pool_write_lock()")
            .expect("drop_dynamic_pool must hold pool_write_lock");
        assert!(
            !body[..lock_pos].contains("is_dynamic_pool(id)"),
            "dynamic membership must not be checked only before pool_write_lock"
        );

        let under_lock = &body[lock_pos..];
        let membership_pos = under_lock
            .find("if !dynamics.contains(id)")
            .expect("drop_dynamic_pool must recheck dynamic membership under lock");
        let remove_pos = under_lock
            .find("new_pools.remove(id)")
            .expect("drop_dynamic_pool must remove from POOLS");
        assert!(
            membership_pos < remove_pos,
            "drop_dynamic_pool must revalidate dynamic membership before removing POOLS entry"
        );
    }

    /// Idempotency under contention: many threads racing on the same
    /// identifier must converge to exactly one membership. With the
    /// old pattern the fast-path `contains` check could be true
    /// for every losing thread, but a writer that observed `false`
    /// concurrently could still race against another writer with the
    /// same observation - both clone the same snapshot, both insert
    /// the same key, both store; the final state contains the key
    /// once but the test below also verifies no spurious second
    /// transition fires (semantically: the post-test DYNAMIC_POOLS
    /// snapshot contains the key exactly once - HashSet semantics
    /// guarantee this trivially, so the test really validates that
    /// no thread crashed / panicked under the new lock).
    #[test]
    fn register_dynamic_pool_idempotent_under_contention() {
        use std::sync::Barrier;
        use std::thread;

        let id = PoolIdentifier::new("h16_idempotent_db", "shared_user");
        // Ensure clean state - other tests must not have registered
        // this exact identifier.
        {
            let _g = pool_write_lock();
            let mut current = (**DYNAMIC_POOLS.load()).clone();
            current.remove(&id);
            DYNAMIC_POOLS.store(Arc::new(current));
        }

        const N: usize = 16;
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let b = Arc::clone(&barrier);
            let id = id.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                register_dynamic_pool(&id);
            }));
        }
        for h in handles {
            h.join().expect("idempotent register thread panicked");
        }
        assert!(
            DYNAMIC_POOLS.load().contains(&id),
            "idempotent register lost the only insert"
        );

        // Cleanup.
        let _g = pool_write_lock();
        let mut current = (**DYNAMIC_POOLS.load()).clone();
        current.remove(&id);
        DYNAMIC_POOLS.store(Arc::new(current));
    }

    /// `canceled_pids_insert` must stop inserting once
    /// `CANCELED_PIDS` reaches `CANCELED_PIDS_CAP` and report `false`
    /// so callers can detect the drop.
    ///
    /// Concurrency note: `CANCELED_PIDS` is a process-global `DashMap`
    /// that other cancel tests touch. `serial_test::serial` with NO key
    /// serialises this against every other globally-serialised test,
    /// and the cancel tests in this audit are the only ones that fill
    /// the set to its cap. We fill to cap, then prove the contract:
    /// once at/above cap, a fresh pid is rejected (`false`) and not
    /// inserted. We do not assert exact `len()` so a stray entry left
    /// by an unrelated test cannot flake the result.
    #[test]
    #[serial_test::serial]
    fn canceled_pids_insert_respects_cap() {
        CANCELED_PIDS.clear();
        // Fill until the helper reports the cap was hit (returns false).
        // Foreign residue only makes this terminate sooner, never later.
        let mut hit_cap = false;
        for pid in 0..(CANCELED_PIDS_CAP as i32 + 16) {
            if !canceled_pids_insert(pid) {
                hit_cap = true;
                break;
            }
        }
        assert!(
            hit_cap,
            "canceled_pids_insert must eventually return false at the cap"
        );
        assert!(
            CANCELED_PIDS.len() >= CANCELED_PIDS_CAP,
            "the set must be at or above CANCELED_PIDS_CAP once a drop occurred"
        );
        // A fresh, never-inserted pid must be rejected and not stored
        // while the set is at cap.
        let over = -777_001;
        CANCELED_PIDS.remove(&over);
        assert!(!canceled_pids_insert(over));
        assert!(
            !CANCELED_PIDS.contains_key(&over),
            "an over-cap pid must not be inserted"
        );
        CANCELED_PIDS.clear();
    }

    /// fail-closed cancel forwarding. `should_forward_cancel` is
    /// the single decision point used by `handle_cancel_mode`: it sets
    /// the quarantine marker and reports whether the cancel may be
    /// forwarded to the backend. When `CANCELED_PIDS` is at cap the
    /// marker is NOT set, so forwarding the cancel would let the backend
    /// get recycled to a different client before the async cancel lands
    /// (cross-client cancel). The decision must therefore be `false`
    /// (drop the cancel) at cap, and `true` (quarantine set, safe to
    /// forward) below cap.
    #[test]
    #[serial_test::serial]
    fn should_forward_cancel_is_fail_closed_at_cap() {
        // Below cap: marker set, forwarding allowed. Use a unique pid so
        // a concurrent cancel test cannot have inserted it already.
        CANCELED_PIDS.clear();
        let below_pid = -424_242;
        CANCELED_PIDS.remove(&below_pid);
        assert!(
            should_forward_cancel(below_pid),
            "below cap the quarantine marker is set, so the cancel may be forwarded"
        );
        assert!(
            CANCELED_PIDS.contains_key(&below_pid),
            "should_forward_cancel must set the quarantine marker on the success path"
        );

        // At cap: marker NOT set, forwarding must be refused. Fill until
        // a drop occurs (helper returns false), tolerating foreign residue.
        CANCELED_PIDS.clear();
        for pid in 0..(CANCELED_PIDS_CAP as i32 + 16) {
            if !canceled_pids_insert(pid) {
                break;
            }
        }
        assert!(
            CANCELED_PIDS.len() >= CANCELED_PIDS_CAP,
            "set must be at or above cap before the fail-closed check"
        );
        let fresh_pid = -313_131;
        CANCELED_PIDS.remove(&fresh_pid);
        assert!(
            !should_forward_cancel(fresh_pid),
            "at cap the quarantine marker cannot be set, so the cancel must be dropped, not forwarded"
        );
        assert!(
            !CANCELED_PIDS.contains_key(&fresh_pid),
            "no quarantine marker should exist for a pid dropped at cap"
        );
        CANCELED_PIDS.clear();
    }

    /// `canceled_pids_consume` distinguishes a FRESH quarantine marker
    /// (evict the backend) from a STALE one (a recycled OS pid - reuse the
    /// healthy backend), and always removes the entry so it cannot linger. The
    /// TTL is injected so both branches are deterministic without sleeping.
    #[test]
    #[serial_test::serial]
    fn canceled_pids_consume_fresh_stale_and_absent() {
        CANCELED_PIDS.clear();
        let pid: ProcessId = -515_001;

        // Fresh: just inserted + huge TTL => Fresh, and the entry is consumed.
        assert!(canceled_pids_insert(pid));
        assert_eq!(
            canceled_pids_consume_with_ttl(pid, Duration::from_secs(3600)),
            CancelMarker::Fresh
        );
        assert!(
            !CANCELED_PIDS.contains_key(&pid),
            "consume must remove the marker"
        );

        // Absent: no marker => Absent.
        assert_eq!(
            canceled_pids_consume_with_ttl(pid, Duration::from_secs(3600)),
            CancelMarker::Absent
        );

        // Stale: marker present but TTL = 0 => any age is stale => Stale, and
        // the stale entry is cleared (recycled-pid quarantine - a recycled pid is not evicted).
        assert!(canceled_pids_insert(pid));
        assert_eq!(
            canceled_pids_consume_with_ttl(pid, Duration::ZERO),
            CancelMarker::Stale
        );
        assert!(
            !CANCELED_PIDS.contains_key(&pid),
            "a stale marker must be cleared so it cannot linger across pid recycling"
        );

        CANCELED_PIDS.clear();
    }
}
