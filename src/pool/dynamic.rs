//! Dynamic pool creation for auth_query passthrough mode.
//!
//! When a client authenticates via `auth_query` in passthrough mode (no `server_user`),
//! pg_doorman creates a per-user pool on the fly. These pools are tracked in `DYNAMIC_POOLS`
//! and garbage-collected when idle. On RELOAD, dynamic pools are dropped and recreated
//! on the next client connection with fresh settings.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use log::{debug, info, warn};

use crate::config::{get_config, BackendAuthMethod, PoolMode, User};
use crate::errors::Error;
use crate::stats::AddressStats;

use super::types::{PoolConfig, QueueMode};
use super::{
    build_server_tls_for_pool, get_auth_query_state, get_coordinator, get_pool,
    resolve_pool_connect_timeout, resolve_pool_timeouts, resolve_server_cache_size, Address,
    CheckQueryCache, ConnectionPool, Pool, PoolIdentifier, PoolSettings, PreparedStatementCache,
    ServerPool, POOLS,
};

const MAX_DYNAMIC_POOLS_PER_DATABASE: usize = 1024;

/// Create a dynamic data pool for auth_query passthrough mode.
/// Returns the new (or existing) pool. Race-safe: if another thread
/// created the pool concurrently, returns the existing one.
///
/// On RELOAD, dynamic pools are dropped (not in config) and recreated
/// on the next client connection with fresh settings.
/// `fetched_overlay` is the per-user `startup_parameters` map from the
/// auth_query row that authenticated this user. Passing it in ties pool
/// creation to that row instead of reading the cache again.
pub fn create_dynamic_pool(
    pool_name: &str,
    username: &str,
    expected_auth_query_state: &Arc<super::AuthQueryState>,
    backend_auth: Option<BackendAuthMethod>,
    fetched_overlay: Arc<std::collections::HashMap<String, String>>,
    fetched_overlay_hash: u64,
) -> Result<(ConnectionPool, super::PoolInitGuard), Error> {
    if !auth_query_state_generation_is_current(pool_name, expected_auth_query_state) {
        return Err(auth_query_state_changed_error(pool_name, username));
    }

    // Fast path: pool already exists. The cache-side refetch path
    // already drops the live pool when an auth_query refetch changes
    // the overlay (see `drop_dynamic_pool_if_overlay_drifted`), but a
    // concurrent login can still arrive after the cache published the
    // fresh entry yet before the drop fires, or with a fetched_overlay
    // newer than what the live pool was frozen with. Check the overlay
    // hash here too so that login rebuilds the pool against the
    // current snapshot instead of inheriting a stale one. The hash is
    // precomputed on `CacheEntry`, so the fast path skips the sort +
    // SipHash on every login.
    if let Some(existing) = get_pool(pool_name, username) {
        let identifier = super::PoolIdentifier::new(pool_name, username);
        let live_hash = existing.per_user_startup_overlay_hash;
        let is_dyn = super::is_dynamic_pool(&identifier);
        if !should_rebuild_for_overlay_drift(live_hash, fetched_overlay_hash, is_dyn) {
            // Hash matches, or the live pool is static and the empty
            // baseline does not match an auth_query overlay — either
            // way the existing pool wins. Refresh `backend_auth` only
            // on hash match: a password rotation between cache
            // fetches still applies, but a static pool is left alone.
            if live_hash == fetched_overlay_hash {
                if let (Some(ref ba_lock), Some(new_ba)) =
                    (&existing.address.backend_auth, &backend_auth)
                {
                    debug!(
                        "[{username}@{pool_name}] auth_query: dynamic pool already exists, updating backend_auth"
                    );
                    *ba_lock.write() = new_ba.clone();
                }
            }
            return Ok((existing, super::PoolInitGuard::already_committed()));
        }
        if super::drop_dynamic_pool(&identifier) {
            info!(
                "[{username}@{pool_name}] auth_query: per-user startup_parameters overlay drift on login — dynamic pool dropped, rebuilding"
            );
        }
    }

    let config = get_config();
    let pool_config = config.pools.get(pool_name).ok_or_else(|| {
        Error::AuthError(format!(
            "auth_query: pool config '{pool_name}' not found for dynamic pool"
        ))
    })?;
    let aq_config = pool_config.auth_query.as_ref().ok_or_else(|| {
        Error::AuthError(format!(
            "auth_query: config not found in pool '{pool_name}' for dynamic pool"
        ))
    })?;
    let client_server_map = super::get_client_server_map()
        .ok_or_else(|| Error::AuthError("auth_query: client_server_map not initialized".into()))?;

    let server_database = pool_config
        .server_database
        .clone()
        .unwrap_or_else(|| pool_name.to_string());

    let ba_arc = backend_auth.map(|ba| Arc::new(parking_lot::RwLock::new(ba)));
    debug!(
        "[{username}@{pool_name}] building server TLS config (mode={})",
        pool_config
            .server_tls_mode
            .as_deref()
            .unwrap_or(&config.general.server_tls_mode)
    );
    let server_tls = build_server_tls_for_pool(pool_config, &config.general)?;

    let address = Address {
        database: pool_name.to_string(),
        host: pool_config.server_host.clone(),
        port: pool_config.server_port,
        username: username.to_string(),
        password: String::new(),
        pool_name: pool_name.to_string(),
        stats: Arc::new(AddressStats::default()),
        backend_auth: ba_arc,
        server_tls,
    };

    let user = User {
        username: username.to_string(),
        password: String::new(),
        pool_size: aq_config.pool_size,
        min_pool_size: if aq_config.min_pool_size > 0 {
            Some(aq_config.min_pool_size)
        } else {
            None
        },
        server_username: Some(username.to_string()),
        server_password: None,
        ..Default::default()
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

    let fallback_state = super::build_fallback_state(pool_name, pool_config, &config.general);

    // Merge general+pool startup_parameters baseline from the same config
    // snapshot. Dynamic auth_query pools follow the same lifecycle as
    // static pools: rebuilt on RELOAD when the underlying base changes
    // (see `general_startup_parameters_changed` in pool/mod.rs).
    let base_startup_parameters = std::sync::Arc::new(
        crate::config::startup_parameters::cascade_canonical_keys(&[
            &config.general.startup_parameters,
            &pool_config.startup_parameters,
        ]),
    );

    // Convert the caller's HashMap snapshot into the BTreeMap shape
    // ServerPool stores. The snapshot comes from the auth_query row used
    // for this login, so TTL expiry or an interleaved refetch cannot
    // change the overlay while the pool is created. Dedicated-mode pools
    // should not reach this path, but keep the guard so a future caller
    // cannot attach a per-user overlay to a shared backend pool.
    let per_user_startup_overlay: std::sync::Arc<std::collections::BTreeMap<String, String>> = {
        let is_dedicated = super::get_auth_query_state(pool_name)
            .map(|state| state.config.is_dedicated_mode())
            .unwrap_or(false);
        if is_dedicated || fetched_overlay.is_empty() {
            std::sync::Arc::new(std::collections::BTreeMap::new())
        } else {
            let map: std::collections::BTreeMap<String, String> = fetched_overlay
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            std::sync::Arc::new(map)
        }
    };

    // Dynamic (passthrough) pool: there is no static user config, so the
    // per-user `prewarm_query` override is unavailable. Pool-level value
    // applies to every dynamically-created passthrough backend.
    let effective_prewarm = pool_config.prewarm_query.clone();
    let connect_timeout = resolve_pool_connect_timeout(pool_config, &config.general);

    let manager = ServerPool::new(
        address.clone(),
        user.clone(),
        server_database.as_str(),
        client_server_map,
        pool_config.cleanup_server_connections,
        pool_config.log_client_parameter_status_changes,
        server_prepared_statements_cache_size,
        application_name,
        config.general.max_concurrent_creates,
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
        per_user_startup_overlay.clone(),
    )
    .with_release_query(pool_config.release_query.clone())
    .with_prewarm_query(effective_prewarm)
    .with_intercept_discard_all(pool_config.intercept_discard_all);

    // The auth_query cache compares the new fetched per-user map against
    // this value after every refetch; a mismatch drops the dynamic pool
    // so the next connect rebuilds with the new reset_val. The caller
    // already has the hash precomputed on the `CacheEntry`, so we reuse
    // it instead of re-running per_user_overlay_hash on the same map.
    let overlay_hash = fetched_overlay_hash;

    let queue_strategy = match config.general.server_round_robin {
        true => QueueMode::Fifo,
        false => QueueMode::Lifo,
    };

    // single coordinator read shared by both the inner
    // builder and the outer `ConnectionPool::coordinator` field.
    // Previously two separate `get_coordinator(pool_name)` calls
    // could observe different `Arc<PoolCoordinator>` instances
    // across a RELOAD that races `from_config` republishing
    // `COORDINATORS`. The two fields would then track different
    // coordinator generations: `retain.rs` reads
    // `min_connection_lifetime_ms` from the outer one and walks
    // reserves whose `coordinator_permit` was issued by the inner
    // one - permit accounting drifts until the pool rotates.
    let coordinator = get_coordinator(pool_name);

    let pool = Pool::builder(manager)
        .coordinator(coordinator.clone())
        .pool_name(pool_name.to_string())
        .username(username.to_string())
        .config(PoolConfig {
            max_size: user.pool_size as usize,
            timeouts: resolve_pool_timeouts(pool_config, &config.general),
            queue_mode: queue_strategy,
            scaling: pool_config.resolve_scaling_config(&config.general),
        })
        .build();

    let conn_pool = ConnectionPool {
        database: pool,
        address,
        config_hash: 0, // dynamic pools don't participate in hash-based reload
        per_user_startup_overlay_hash: overlay_hash,
        original_server_parameters: Arc::new(tokio::sync::OnceCell::new()),
        settings: PoolSettings {
            pool_mode,
            user,
            db: pool_name.to_string(),
            idle_timeout_ms: pool_config
                .idle_timeout
                .unwrap_or(config.general.idle_timeout.as_millis()),
            life_time_ms: pool_config
                .server_lifetime
                .unwrap_or(config.general.server_lifetime.as_millis()),
            sync_server_parameters: config.general.sync_server_parameters,
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
        coordinator,
        replenish_failures: Arc::new(AtomicU32::new(0)),
        init_complete: Arc::new(AtomicBool::new(false)),
    };

    // Atomic insert into POOLS.
    //
    // take the write-serialisation lock so the POOLS.store +
    // register_dynamic_pool below cannot interleave with another
    // dynamic-pool insert (for a different user), a RELOAD that
    // republishes POOLS, a drop_dynamic_pool, or a GC sweep. Without
    // this, two concurrent inserts both `load()` the same snapshot and
    // one's insert is silently dropped when the other stores its clone
    // (last-write-wins). All POOLS mutations after this point are guarded.
    // parking_lot::Mutex is not reentrant, so register_dynamic_pool below
    // must observe the same hold - it has an internal fast-path that
    // returns before re-locking. Instead, do the DYNAMIC_POOLS insert
    // inline while we still hold the lock.
    let identifier = PoolIdentifier::new(pool_name, username);
    let _commit_guard = super::pool_write_lock();
    if !auth_query_state_generation_is_current(pool_name, expected_auth_query_state) {
        return Err(auth_query_state_changed_error(pool_name, username));
    }
    let current = POOLS.load();
    let mut new_pools = (**current).clone();
    let mut removed_drifted_pool = None;

    // Re-check after clone (another thread may have created it). The
    // fast path at the top of this function already validates the
    // overlay hash; do the same here so the slow path doesn't reuse a
    // pool another login built with a stale `startup_parameters`
    // snapshot. Without this compare, two concurrent logins after an
    // auth_query row update can race: one wins the slow path with the
    // new overlay, the other finds the loser's `existing` and inherits
    // the stale `reset_val` until TTL or RELOAD.
    if let Some(existing) = new_pools.get(&identifier) {
        let live_hash = existing.per_user_startup_overlay_hash;
        let is_dyn = super::is_dynamic_pool(&identifier);
        if !should_rebuild_for_overlay_drift(live_hash, overlay_hash, is_dyn) {
            // Same reasoning as the fast path: refresh backend_auth
            // only when the live pool is a hash-matching dynamic. A
            // static pool registered concurrently with the in-flight
            // dynamic-pool build is preserved unchanged.
            if live_hash == overlay_hash {
                if let (Some(ref ba_lock), Some(ref new_ba)) = (
                    &existing.address.backend_auth,
                    &conn_pool.address.backend_auth,
                ) {
                    *ba_lock.write() = new_ba.read().clone();
                }
            }
            return Ok((existing.clone(), super::PoolInitGuard::already_committed()));
        }
        info!(
            "[{username}@{pool_name}] auth_query: per-user startup_parameters overlay drift on slow-path race — replacing concurrently-built pool"
        );
        removed_drifted_pool = new_pools.remove(&identifier);
        if let Some(pool) = &removed_drifted_pool {
            pool.database.close_new_checkouts();
        }
    }

    let current_dyn = super::DYNAMIC_POOLS.load();
    if !dynamic_pool_insert_allowed(current_dyn.as_ref(), pool_name, &identifier) {
        warn!(
            "[{username}@{pool_name}] auth_query: dynamic pool limit reached \
             ({MAX_DYNAMIC_POOLS_PER_DATABASE} per database)"
        );
        return Err(Error::AuthError(format!(
            "auth_query: too many dynamic users for pool '{pool_name}' \
             (limit {MAX_DYNAMIC_POOLS_PER_DATABASE})"
        )));
    }

    let auth_method = match &conn_pool.address.backend_auth {
        Some(ba) => {
            let guard = ba.read();
            match &*guard {
                BackendAuthMethod::Md5PassTheHash(_) => "md5-pass-the-hash",
                BackendAuthMethod::ScramPassthrough(_) => "scram-passthrough",
                BackendAuthMethod::ScramPending => "scram-pending",
            }
        }
        None => "none",
    };
    info!("[{username}@{pool_name}] dynamic pool created (backend_auth={auth_method})");
    new_pools.insert(identifier.clone(), conn_pool.clone());

    // Publish `DYNAMIC_POOLS` before `POOLS`. Hot readers in `auth/mod.rs`
    // call `get_pool()` followed by `is_dynamic_pool()`; any reader that
    // sees the pool via `get_pool()` must also see it as dynamic and route
    // through `try_auth_query`. The write_lock serialises writers; the
    // ordering above only matters for ArcSwap readers that bypass
    // the lock.
    {
        let current_dyn = super::DYNAMIC_POOLS.load();
        if !current_dyn.contains(&identifier) {
            let mut new_dyn = (**current_dyn).clone();
            new_dyn.insert(identifier.clone());
            super::DYNAMIC_POOLS.store(Arc::new(new_dyn));
        }
    }
    POOLS.store(Arc::new(new_pools));
    drop(_commit_guard);
    if let Some(pool) = &removed_drifted_pool {
        pool.database.close();
    }

    // Prewarm: spawn background task to create min_pool_size connections.
    //
    // check `is_dynamic_pool` membership before replenishing.
    // If RELOAD drops the dynamic pool between insert and prewarm
    // (e.g. `general_startup_parameters_changed`), the spawned task
    // would otherwise create orphan backends that never serve traffic
    // and that accumulate against `max_concurrent_creates` until
    // `lifetime_ms` ages them out.
    if aq_config.min_pool_size > 0 {
        let pool_clone = conn_pool.clone();
        let min = aq_config.min_pool_size as usize;
        let pn = pool_name.to_string();
        let un = username.to_string();
        let identifier_for_check = identifier.clone();
        let generation_for_check = Arc::clone(&conn_pool.init_complete);
        tokio::spawn(async move {
            if !dynamic_pool_generation_is_current(&identifier_for_check, &generation_for_check) {
                warn!(
                    "[{un}@{pn}] dynamic prewarm aborted: pool was removed before \
                     prewarm could start (RELOAD race)"
                );
                return;
            }
            let created = pool_clone.database.replenish(min).await;
            if created > 0 {
                info!("[{un}@{pn}] prewarmed {created} dynamic server(s) (min_pool_size={min})");
            } else {
                warn!("[{un}@{pn}] dynamic prewarm failed: 0 of {min} connections created");
            }
        });
    }

    // Increment dynamic_pools_created stat
    if let Some(state) = get_auth_query_state(pool_name) {
        state
            .stats
            .dynamic_pools_created
            .fetch_add(1, Ordering::Relaxed);
    }

    let guard =
        super::PoolInitGuard::for_new_pool(identifier, Arc::clone(&conn_pool.init_complete));
    Ok((conn_pool, guard))
}

/// Decide whether `create_dynamic_pool` should replace an existing
/// `(pool, user)` entry in `POOLS`. Hash drift alone is not enough —
/// a static pool registered for the same identifier (during a config
/// reload race with an in-flight auth_query login) keeps the empty
/// overlay hash, and replacing it would silently swap the operator's
/// configured backend auth/startup-parameters for the auth_query
/// passthrough version. Rebuild only when the live pool is dynamic.
fn should_rebuild_for_overlay_drift(live_hash: u64, fetched_hash: u64, is_dynamic: bool) -> bool {
    live_hash != fetched_hash && is_dynamic
}

fn dynamic_pool_generation_is_current(
    identifier: &PoolIdentifier,
    init_complete: &Arc<AtomicBool>,
) -> bool {
    let pools = POOLS.load();
    let Some(pool) = pools.get(identifier) else {
        return false;
    };
    if !Arc::ptr_eq(&pool.init_complete, init_complete) {
        return false;
    }
    super::DYNAMIC_POOLS.load().contains(identifier)
}

fn auth_query_state_generation_is_current(
    pool_name: &str,
    expected_auth_query_state: &Arc<super::AuthQueryState>,
) -> bool {
    super::get_auth_query_state(pool_name)
        .as_ref()
        .map(|current| Arc::ptr_eq(current, expected_auth_query_state))
        .unwrap_or(false)
}

fn auth_query_state_changed_error(pool_name: &str, username: &str) -> Error {
    warn!(
        "[{username}@{pool_name}] auth_query state changed during dynamic pool creation; \
         rejecting stale dynamic pool publish after reload"
    );
    Error::AuthError(format!(
        "auth_query state changed during dynamic pool creation for pool '{pool_name}'"
    ))
}

fn dynamic_pool_insert_allowed(
    dynamic_ids: &HashSet<PoolIdentifier>,
    pool_name: &str,
    candidate: &PoolIdentifier,
) -> bool {
    if dynamic_ids.contains(candidate) {
        return true;
    }
    dynamic_ids
        .iter()
        .filter(|id| id.db == pool_name)
        .take(MAX_DYNAMIC_POOLS_PER_DATABASE)
        .count()
        < MAX_DYNAMIC_POOLS_PER_DATABASE
}

#[cfg(test)]
mod tests {
    use super::{
        dynamic_pool_generation_is_current, dynamic_pool_insert_allowed,
        should_rebuild_for_overlay_drift,
    };
    use crate::pool::{pool_write_lock, ConnectionPool, PoolIdentifier, DYNAMIC_POOLS, POOLS};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[test]
    fn dynamic_prewarm_checks_current_published_generation() {
        let src = include_str!("dynamic.rs");
        let prewarm_start = src.find("Prewarm: spawn background task").unwrap();
        let prewarm_block = &src[prewarm_start..];
        let prewarm_end = prewarm_block
            .find("Increment dynamic_pools_created stat")
            .expect("dynamic prewarm block end not found");
        let prewarm_block = &prewarm_block[..prewarm_end];

        assert!(
            prewarm_block.contains("dynamic_pool_generation_is_current"),
            "dynamic prewarm must verify the captured pool generation, not only \
             DYNAMIC_POOLS membership for the same identifier"
        );
    }

    #[test]
    fn overlay_drift_reuses_on_hash_match() {
        let h = 0x1234_5678_9abc_def0_u64;
        assert!(!should_rebuild_for_overlay_drift(h, h, true));
        assert!(!should_rebuild_for_overlay_drift(h, h, false));
    }

    #[test]
    fn overlay_drift_rebuilds_dynamic_on_hash_mismatch() {
        assert!(should_rebuild_for_overlay_drift(0xAAAA, 0xBBBB, true));
    }

    #[test]
    fn overlay_drift_preserves_static_on_hash_mismatch() {
        // A static pool registered during reload races with an
        // in-flight auth_query login that fetched a non-empty overlay.
        // The live pool's hash is `empty_overlay_hash()`; the fetched
        // hash is non-empty. Static-overrides-dynamic must hold, so
        // the existing pool wins and is not replaced.
        let empty = crate::pool::empty_overlay_hash();
        let fetched = 0xBEEF_0000_0000_0001_u64;
        assert_ne!(empty, fetched);
        assert!(!should_rebuild_for_overlay_drift(
            empty, fetched, /*is_dynamic=*/ false
        ));
    }

    #[test]
    fn dynamic_pool_generation_rejects_stale_same_identifier_pool() {
        let id = PoolIdentifier::new("dynamic_generation_db", "dynamic_user");
        let stale_generation = Arc::new(AtomicBool::new(true));
        let live_generation = Arc::new(AtomicBool::new(true));
        let mut live_pool = ConnectionPool::test_for_protocol();
        live_pool.init_complete = Arc::clone(&live_generation);

        {
            let _guard = pool_write_lock();
            let mut pools = (**POOLS.load()).clone();
            pools.insert(id.clone(), live_pool);
            POOLS.store(Arc::new(pools));

            let mut dynamics = (**DYNAMIC_POOLS.load()).clone();
            dynamics.insert(id.clone());
            DYNAMIC_POOLS.store(Arc::new(dynamics));
        }

        assert!(
            dynamic_pool_generation_is_current(&id, &live_generation),
            "the live dynamic pool generation must be accepted"
        );
        assert!(
            !dynamic_pool_generation_is_current(&id, &stale_generation),
            "a removed generation with the same identifier must not be accepted"
        );

        let _guard = pool_write_lock();
        let mut pools = (**POOLS.load()).clone();
        pools.remove(&id);
        POOLS.store(Arc::new(pools));
        let mut dynamics = (**DYNAMIC_POOLS.load()).clone();
        dynamics.remove(&id);
        DYNAMIC_POOLS.store(Arc::new(dynamics));
    }

    #[test]
    fn slow_path_overlay_replacement_closes_removed_generation_before_publish() {
        let src = include_str!("dynamic.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let slow_path_start = impl_src
            .find("per-user startup_parameters overlay drift on slow-path race")
            .expect("slow-path overlay drift replacement block not found");
        let slow_path_block = &impl_src[slow_path_start..];
        let publish_idx = slow_path_block
            .find("POOLS.store")
            .expect("dynamic slow path must publish POOLS");
        let before_publish = &slow_path_block[..publish_idx];

        assert!(
            before_publish.contains("close_new_checkouts"),
            "dynamic slow-path overlay replacement must close removed generations \
             before publishing a POOLS map with the replacement"
        );

        let after_publish = &slow_path_block[publish_idx..];
        let unlock_idx = after_publish
            .find("drop(_commit_guard)")
            .expect("dynamic slow path must release pool_write_lock explicitly");
        let after_unlock = &after_publish[unlock_idx..];
        assert!(
            after_unlock.contains(".database.close()"),
            "dynamic slow-path overlay replacement must close/drain removed generations \
             after releasing pool_write_lock"
        );
    }

    #[test]
    fn dynamic_pool_create_rechecks_auth_query_generation_under_commit_lock() {
        let src = include_str!("dynamic.rs");
        let fn_start = src
            .find("pub fn create_dynamic_pool(")
            .expect("create_dynamic_pool must exist");
        let body = &src[fn_start..];
        let fn_end = body
            .find("\n/// Decide whether `create_dynamic_pool`")
            .expect("create_dynamic_pool helper marker must follow function");
        let body = &body[..fn_end];

        assert!(
            body.contains("expected_auth_query_state: &Arc<super::AuthQueryState>"),
            "dynamic pool creation must carry the AuthQueryState that supplied \
             the authenticated cache row"
        );

        let lock_idx = body
            .find("let _commit_guard = super::pool_write_lock()")
            .expect("dynamic pool create must hold pool_write_lock before publish");
        let recheck_idx = body[lock_idx..]
            .find("auth_query_state_generation_is_current(pool_name, expected_auth_query_state)")
            .map(|offset| lock_idx + offset)
            .expect("dynamic pool create must recheck auth_query generation under the commit lock");
        let dynamic_publish_idx = body[recheck_idx..]
            .find("DYNAMIC_POOLS.store")
            .map(|offset| recheck_idx + offset)
            .expect("dynamic pool create must publish DYNAMIC_POOLS");
        let pools_publish_idx = body[recheck_idx..]
            .find("POOLS.store")
            .map(|offset| recheck_idx + offset)
            .expect("dynamic pool create must publish POOLS");

        assert!(
            lock_idx < recheck_idx
                && recheck_idx < dynamic_publish_idx
                && recheck_idx < pools_publish_idx,
            "dynamic pool create must reject stale auth_query generations before publishing either map"
        );
        assert!(
            body[recheck_idx..].contains("auth_query_state_changed_error(pool_name, username)"),
            "stale auth_query generation rejection must use the dedicated reload-race error"
        );
        assert!(
            src.contains("auth_query state changed during dynamic pool creation"),
            "stale auth_query generation rejection must explain the reload race"
        );
    }

    #[test]
    fn dynamic_pool_limit_rejects_new_user_but_allows_existing_id() {
        let pool_name = "limited_db";
        let mut dynamic_ids = HashSet::new();
        for i in 0..super::MAX_DYNAMIC_POOLS_PER_DATABASE {
            dynamic_ids.insert(PoolIdentifier::new(pool_name, &format!("u_{i}")));
        }

        let new_user = PoolIdentifier::new(pool_name, "new_user");
        assert!(
            !dynamic_pool_insert_allowed(&dynamic_ids, pool_name, &new_user),
            "new dynamic users must be rejected once the per-db cap is reached"
        );

        let existing_user = PoolIdentifier::new(pool_name, "u_0");
        assert!(
            dynamic_pool_insert_allowed(&dynamic_ids, pool_name, &existing_user),
            "existing dynamic pool rebuild/reuse must not be blocked by the cap"
        );

        let other_db_user = PoolIdentifier::new("other_db", "new_user");
        assert!(
            dynamic_pool_insert_allowed(&dynamic_ids, "other_db", &other_db_user),
            "dynamic pools in other databases must not count against this database"
        );
    }
}
