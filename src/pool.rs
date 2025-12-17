use arc_swap::ArcSwap;
use deadpool::{managed, Runtime};
use log::{error, info, warn};
use lru::LruCache;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{get_config, Address, General, PoolMode, User};
use crate::errors::Error;
use crate::messages::Parse;

use crate::server::{Server, ServerParameters};
use crate::stats::{AddressStats, ServerStats};

tokio::task_local! {
    /// Suffix to append to application_name for connections created in this task.
    /// Used by warm_up to identify pre-warmed connections.
    pub static APPLICATION_NAME_SUFFIX: String;
}

pub type ProcessId = i32;
pub type SecretKey = i32;
pub type ServerHost = String;
pub type ServerPort = u16;

pub type ClientServerMap =
    Arc<Mutex<HashMap<(ProcessId, SecretKey), (ProcessId, SecretKey, ServerHost, ServerPort)>>>;
pub type PoolMap = HashMap<PoolIdentifierVirtual, ConnectionPool>;

/// The connection pool, globally available.
/// This is atomic and safe and read-optimized.
/// The pool is recreated dynamically when the config is reloaded.
pub static POOLS: Lazy<ArcSwap<PoolMap>> = Lazy::new(|| ArcSwap::from_pointee(HashMap::default()));
pub static CANCELED_PIDS: Lazy<Arc<Mutex<HashSet<ProcessId>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashSet::new())));

pub type PreparedStatementCacheType = Arc<Mutex<PreparedStatementCache>>;
pub type ServerParametersType = Arc<tokio::sync::Mutex<ServerParameters>>;

// TODO: Add stats the this cache
// TODO: Add application name to the cache value to help identify which application is using the cache
// TODO: Create admin command to show which statements are in the cache
#[derive(Debug)]
pub struct PreparedStatementCache {
    cache: LruCache<u64, Arc<Parse>>,
}

impl PreparedStatementCache {
    pub fn new(mut size: usize) -> Self {
        // Cannot be zeros
        if size == 0 {
            size = 1;
        }

        PreparedStatementCache {
            cache: LruCache::new(NonZeroUsize::new(size).unwrap()),
        }
    }

    /// Adds the prepared statement to the cache if it doesn't exist with a new name
    /// if it already exists will give you the existing parse
    ///
    /// Pass the hash to this so that we can do the compute before acquiring the lock
    pub fn get_or_insert(&mut self, parse: &Parse, hash: u64) -> Arc<Parse> {
        match self.cache.get(&hash) {
            Some(rewritten_parse) => rewritten_parse.clone(),
            None => {
                let new_parse = Arc::new(parse.clone().rewrite());
                let evicted = self.cache.push(hash, new_parse.clone());

                if let Some((_, evicted_parse)) = evicted {
                    warn!(
                        "Evicted prepared statement {} from cache",
                        evicted_parse.name
                    );
                }

                new_parse
            }
        }
    }

    /// Marks the hash as most recently used if it exists
    pub fn promote(&mut self, hash: &u64) {
        self.cache.promote(hash);
    }
}

/// An identifier for a PgDoorman pool,
/// a virtual database pool.
#[derive(Hash, Debug, Clone, PartialEq, Eq, Default)]
pub struct PoolIdentifierVirtual {
    // The name of the database clients want to connect to.
    pub db: String,

    // The username the client connects with. Each user gets its own pool.
    pub user: String,

    // Virtual pool ID
    pub virtual_pool_id: u16,
}

/// An identifier for a PgDoorman pool,
/// a real database visible to clients.
/// Used for statistics.
#[derive(Hash, Debug, Clone, PartialEq, Eq, Default)]
pub struct StatsPoolIdentifier {
    pub db: String,
    pub user: String,
}

impl StatsPoolIdentifier {
    pub fn contains(self, p: PoolIdentifierVirtual) -> bool {
        self.db == p.db && self.user == p.user
    }
}

impl PoolIdentifierVirtual {
    /// Create a new user/pool identifier.
    pub fn new(db: &str, user: &str, virtual_pool_id: u16) -> PoolIdentifierVirtual {
        PoolIdentifierVirtual {
            db: db.to_string(),
            user: user.to_string(),
            virtual_pool_id,
        }
    }
}

impl Display for PoolIdentifierVirtual {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.user, self.db)
    }
}

impl From<&Address> for PoolIdentifierVirtual {
    fn from(address: &Address) -> PoolIdentifierVirtual {
        PoolIdentifierVirtual::new(
            &address.database,
            &address.username,
            address.virtual_pool_id,
        )
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

    /// Synchronize server parameters set by client via SET. (False).
    pub sync_server_parameters: bool,

    idle_timeout_ms: u64,
    life_time_ms: u64,

    /// Minimum pool size for this virtual pool (already divided by virtual_pool_count).
    min_pool_size: usize,
}

impl Default for PoolSettings {
    fn default() -> PoolSettings {
        PoolSettings {
            pool_mode: PoolMode::Transaction,
            user: User::default(),
            db: String::default(),
            idle_timeout_ms: General::default_idle_timeout(),
            life_time_ms: General::default_server_lifetime(),
            sync_server_parameters: General::default_sync_server_parameters(),
            min_pool_size: 0,
        }
    }
}

/// The globally accessible connection pool.
#[derive(Clone, Debug)]
pub struct ConnectionPool {
    /// The pool.
    pub database: managed::Pool<ServerPool>,

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

    /// Cache
    pub prepared_statement_cache: Option<PreparedStatementCacheType>,
}

impl ConnectionPool {
    /// Construct the connection pool from the configuration.
    pub async fn from_config(client_server_map: ClientServerMap) -> Result<(), Error> {
        let config = get_config();

        let mut new_pools = HashMap::new();

        for (pool_name, pool_config) in &config.pools {
            let new_pool_hash_value = pool_config.hash_value();

            // There is one pool per database/user pair.
            for user in pool_config.users.values() {
                for virtual_pool_id in 0..config.general.virtual_pool_count {
                    let old_pool_ref = get_pool(pool_name, &user.username, virtual_pool_id);
                    let identifier =
                        PoolIdentifierVirtual::new(pool_name, &user.username, virtual_pool_id);

                    if let Some(pool) = old_pool_ref {
                        // If the pool hasn't changed, get existing reference and insert it into the new_pools.
                        // We replace all pools at the end, but if the reference is kept, the pool won't get re-created (bb8).
                        if pool.config_hash == new_pool_hash_value {
                            info!(
                                "[pool: {}][user: {}] has not changed",
                                pool_name, user.username
                            );
                            new_pools.insert(identifier.clone(), pool.clone());
                            continue;
                        }
                    }

                    info!(
                        "Creating new pool {}@{}-{}",
                        user.username, pool_name, virtual_pool_id
                    );

                    // real database name on postgresql server.
                    let server_database = pool_config
                        .server_database
                        .clone()
                        .unwrap_or(pool_name.clone().to_string());

                    let address = Address {
                        database: pool_name.clone(),
                        host: pool_config.server_host.clone(),
                        port: pool_config.server_port,
                        virtual_pool_id,
                        username: user.username.clone(),
                        password: user.password.clone(),
                        pool_name: pool_name.clone(),
                        stats: Arc::new(AddressStats::default()),
                    };

                    let prepared_statements_cache_size = match config.general.prepared_statements {
                        true => pool_config
                            .prepared_statements_cache_size
                            .unwrap_or(config.general.prepared_statements_cache_size),
                        false => 0,
                    };

                    let application_name = pool_config
                        .application_name
                        .clone()
                        .unwrap_or_else(|| "pg_doorman".to_string());

                    let manager = ServerPool::new(
                        address.clone(),
                        user.clone(),
                        server_database.as_str(),
                        client_server_map.clone(),
                        pool_config.cleanup_server_connections,
                        pool_config.log_client_parameter_status_changes,
                        prepared_statements_cache_size,
                        application_name,
                    );

                    let queue_strategy = if config.general.oldest_first {
                        managed::QueueMode::OldestFirst
                    } else if config.general.server_round_robin {
                        managed::QueueMode::Fifo
                    } else {
                        managed::QueueMode::Lifo
                    };

                    info!(
                        "[pool: {}][user: {}][vpid: {}]",
                        pool_name, user.username, virtual_pool_id
                    );

                    let max_size = (user.pool_size / config.general.virtual_pool_count as u32)
                        as usize;

                    let mut builder_config = managed::Pool::builder(manager);
                    builder_config = builder_config.config(managed::PoolConfig {
                        max_size,
                        timeouts: managed::Timeouts {
                            wait: Some(Duration::from_millis(config.general.query_wait_timeout)),
                            create: Some(Duration::from_millis(config.general.connect_timeout)),
                            recycle: None,
                        },
                        queue_mode: queue_strategy,
                    });
                    builder_config = builder_config.runtime(Runtime::Tokio1);

                    let pool = match builder_config.build() {
                        Ok(p) => p,
                        Err(err) => {
                            error!("error build pool: {err:?}");
                            return Err(Error::BadConfig(format!("error build pool: {err:?}")));
                        }
                    };

                    // Calculate min_pool_size for this virtual pool (round up to avoid losing connections)
                    // but clamp to max_size to avoid impossible targets
                    let min_pool_size_raw = user
                        .min_pool_size
                        .unwrap_or(0)
                        .div_ceil(config.general.virtual_pool_count as u32)
                        as usize;
                    let min_pool_size = min_pool_size_raw.min(max_size);

                    // Warn if min_pool_size was clamped - effective minimum will be less than configured
                    if min_pool_size_raw > max_size {
                        let effective_total = min_pool_size * config.general.virtual_pool_count as usize;
                        warn!(
                            "[pool: {}][user: {}] min_pool_size ({}) exceeds pool_size ({}), \
                            effective minimum will be {} (clamped to max_size per virtual pool)",
                            pool_name,
                            user.username,
                            user.min_pool_size.unwrap_or(0),
                            user.pool_size,
                            effective_total
                        );
                    }

                    let pool = ConnectionPool {
                        database: pool,
                        address,
                        config_hash: new_pool_hash_value,
                        original_server_parameters: Arc::new(tokio::sync::Mutex::new(
                            ServerParameters::new(),
                        )),
                        settings: PoolSettings {
                            pool_mode: user.pool_mode.unwrap_or(pool_config.pool_mode),
                            user: user.clone(),
                            db: pool_name.clone(),
                            idle_timeout_ms: config.general.idle_timeout,
                            life_time_ms: config.general.server_lifetime,
                            sync_server_parameters: config.general.sync_server_parameters,
                            min_pool_size,
                        },
                        prepared_statement_cache: match config.general.prepared_statements {
                            false => None,
                            true => Some(Arc::new(Mutex::new(PreparedStatementCache::new(
                                config.general.prepared_statements_cache_size,
                            )))),
                        },
                    };

                    // There is one pool per database/user pair.
                    new_pools.insert(
                        PoolIdentifierVirtual::new(pool_name, &user.username, virtual_pool_id),
                        pool,
                    );
                }
            }
        }

        POOLS.store(Arc::new(new_pools.clone()));
        Ok(())
    }

    /// Get pool state for a particular shard server as reported by pooler.
    #[inline(always)]
    pub fn pool_state(&self) -> managed::Status {
        self.database.status()
    }

    pub fn retain_pool_connections(&self, count: Arc<AtomicUsize>, max: usize) {
        let min_pool_size = self.settings.min_pool_size;
        let idle_timeout_ms = self.settings.idle_timeout_ms;
        let life_time_ms = self.settings.life_time_ms;
        let remaining = max.saturating_sub(count.load(Ordering::Relaxed));

        // Use retain_sorted to remove youngest connections first, keeping oldest ones.
        // This preserves connections that have accumulated state (like temp tables).
        // Sort key is age: younger connections have smaller age and will be removed first.
        let removed = self.database.retain_sorted(
            |_, metrics| {
                // Check if connection should be removed (idle timeout or lifetime exceeded)
                // A value of 0 disables the respective timeout
                let idle_expired = idle_timeout_ms > 0
                    && metrics
                        .recycled
                        .map(|v| (v.elapsed().as_millis() as u64) > idle_timeout_ms)
                        .unwrap_or(false);
                let lifetime_expired =
                    life_time_ms > 0 && (metrics.age().as_millis() as u64) > life_time_ms;
                idle_expired || lifetime_expired
            },
            |metrics| {
                // Sort by age ascending (youngest first = smallest age = removed first)
                // We want to keep the OLDEST connections, so remove the YOUNGEST ones first
                metrics.age()
            },
            min_pool_size,
            remaining,
        );

        count.fetch_add(removed, Ordering::Relaxed);
    }

    /// Get the address information for a server.
    #[inline(always)]
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Register a parse statement to the pool's cache and return the rewritten parse
    ///
    /// Do not pass an anonymous parse statement to this function
    #[inline(always)]
    pub fn register_parse_to_cache(&self, hash: u64, parse: &Parse) -> Option<Arc<Parse>> {
        // We should only be calling this function if the cache is enabled
        match self.prepared_statement_cache {
            Some(ref prepared_statement_cache) => {
                let mut cache = prepared_statement_cache.lock();
                Some(cache.get_or_insert(parse, hash))
            }
            None => None,
        }
    }

    /// Promote a prepared statement hash in the LRU
    #[inline(always)]
    pub fn promote_prepared_statement_hash(&self, hash: &u64) {
        // We should only be calling this function if the cache is enabled
        if let Some(ref prepared_statement_cache) = self.prepared_statement_cache {
            let mut cache = prepared_statement_cache.lock();
            cache.promote(hash);
        }
    }

    pub async fn get_server_parameters(&mut self) -> Result<ServerParameters, Error> {
        let mut guard = self.original_server_parameters.lock().await;
        if !guard.is_empty() {
            return Ok(guard.clone());
        }
        info!(
            "Fetching new server parameters from server: {}",
            self.address
        );
        {
            let conn = match self.database.get().await {
                Ok(conn) => conn,
                Err(err) => return Err(Error::ServerStartupReadParameters(err.to_string())),
            };
            guard.set_from_hashmap(conn.server_parameters_as_hashmap(), true);
        }
        Ok(guard.clone())
    }

    /// Pre-warm the pool by creating connections up to min_pool_size.
    /// Avoids spawning many tasks while still forcing new connections to be created.
    /// Returns the number of connections successfully created.
    pub async fn warm_up(&self, log_on_startup: bool) -> usize {
        let min_pool_size = self.settings.min_pool_size;
        if min_pool_size == 0 {
            return 0;
        }

        let size_before = self.pool_state().size;
        let connections_to_create = min_pool_size.saturating_sub(size_before);

        if connections_to_create == 0 {
            return 0;
        }

        if log_on_startup {
            info!(
                "[pool: {}][user: {}] Pre-warming pool with {} connections (min_pool_size: {})",
                self.address.pool_name, self.address.username, connections_to_create, min_pool_size
            );
        }

        // Set APPLICATION_NAME_SUFFIX to identify warm_up connections in pg_stat_activity.
        //
        // Note: server creation is already serialized inside `ServerPool::create()` by
        // `open_new_server`, so spawning many tasks here only adds overhead and memory usage.
        //
        // We keep checked-out connections alive until the end so the pool can't just reuse a
        // single idle connection repeatedly.
        let (error_count, checked_out) = APPLICATION_NAME_SUFFIX
            .scope("warm_up".to_string(), async {
                let mut error_count = 0;
                let mut checked_out = Vec::with_capacity(connections_to_create);
                for _ in 0..connections_to_create {
                    match self.database.get().await {
                        Ok(conn) => checked_out.push(conn),
                        Err(err) => {
                            error_count += 1;
                            warn!(
                                "[pool: {}][user: {}] Failed to create connection for min_pool_size: {:?}",
                                self.address.pool_name, self.address.username, err
                            );
                        }
                    }
                }
                (error_count, checked_out)
            })
            .await;
        drop(checked_out);

        // Calculate actual connections created by comparing pool size before and after
        let size_after = self.pool_state().size;
        let actually_created = size_after.saturating_sub(size_before);

        if log_on_startup {
            if error_count > 0 {
                warn!(
                    "[pool: {}][user: {}] Pre-warming completed: {} created, {} failed",
                    self.address.pool_name, self.address.username, actually_created, error_count
                );
            } else if actually_created > 0 {
                info!(
                    "[pool: {}][user: {}] Pre-warming completed: {} connections created",
                    self.address.pool_name, self.address.username, actually_created
                );
            }
        }

        actually_created
    }
}

/// Wrapper for the connection pool.
#[derive(Debug)]
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

    /// Lock to limit of server connections creating concurrently.
    open_new_server: Arc<tokio::sync::Mutex<u64>>,
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
    ) -> ServerPool {
        ServerPool {
            address,
            user: user.clone(),
            database: database.to_string(),
            client_server_map,
            cleanup_connections,
            log_client_parameter_status_changes,
            prepared_statement_cache_size,
            open_new_server: Arc::new(tokio::sync::Mutex::new(0)),
            application_name,
        }
    }
}

impl managed::Manager for ServerPool {
    type Type = Server;
    type Error = Error;

    /// Attempts to create a new connection.
    async fn create(&self) -> Result<Self::Type, Self::Error> {
        let mut guard = self.open_new_server.lock().await;
        *guard += 1;
        info!(
            "Creating a new server connection to {}[#{}]",
            self.address, guard
        );
        let stats = Arc::new(ServerStats::new(
            self.address.clone(),
            tokio::time::Instant::now(),
        ));

        stats.register(stats.clone());

        // Check if there's a suffix to append to application_name (used by warm_up)
        let application_name = APPLICATION_NAME_SUFFIX
            .try_with(|suffix| format!("{}|{}", self.application_name, suffix))
            .unwrap_or_else(|_| self.application_name.clone());

        // Connect to the PostgreSQL server.
        match Server::startup(
            &self.address,
            &self.user,
            &self.database,
            self.client_server_map.clone(),
            stats.clone(),
            self.cleanup_connections,
            self.log_client_parameter_status_changes,
            self.prepared_statement_cache_size,
            application_name,
        )
        .await
        {
            Ok(conn) => {
                // max rate limit 1 server connection per 10 ms.
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(guard);
                conn.stats.idle(0);
                Ok(conn)
            }
            Err(err) => {
                // if server feels bad sleep more.
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(guard);
                stats.disconnect();
                Err(err)
            }
        }
    }

    async fn recycle(
        &self,
        conn: &mut Server,
        _: &managed::Metrics,
    ) -> managed::RecycleResult<Error> {
        if conn.is_bad() {
            return Err(managed::RecycleError::StaticMessage("Bad connection"));
        }
        Ok(())
    }
}

/// Get the connection pool
pub fn get_pool(db: &str, user: &str, virtual_pool_id: u16) -> Option<ConnectionPool> {
    (*(*POOLS.load()))
        .get(&PoolIdentifierVirtual::new(db, user, virtual_pool_id))
        .cloned()
}

/// Get a pointer to all configured pools.
pub fn get_all_pools() -> HashMap<PoolIdentifierVirtual, ConnectionPool> {
    (*(*POOLS.load())).clone()
}

pub async fn retain_connections() {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
    let count = Arc::new(AtomicUsize::new(0));
    loop {
        interval.tick().await;
        for (_, pool) in get_all_pools() {
            pool.retain_pool_connections(count.clone(), 20);
        }
        count.store(0, Ordering::Relaxed);
    }
}

/// Pre-warm all pools that have min_pool_size configured.
/// This should be called after pool creation to ensure minimum connections are ready.
pub async fn warm_up_pools() {
    let pools = get_all_pools();
    let mut handles = Vec::new();
    for pool in pools.values() {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move { pool.warm_up(true).await }));
    }
    for handle in handles {
        let _ = handle.await;
    }
}

/// Background task that periodically checks and maintains min_pool_size for all pools.
/// This ensures that pools are replenished if connections are lost due to server restarts,
/// network issues, or connection lifetime expiration.
pub async fn maintain_min_pool_size() {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        for (id, pool) in get_all_pools() {
            let created = pool.warm_up(false).await;
            if created > 0 {
                info!(
                    "[pool: {}][user: {}] Replenished {} connections to maintain min_pool_size",
                    id.db, id.user, created
                );
            }
        }
    }
}
