use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info};

use super::{ConnectionPool, PoolIdentifier, Status, AUTH_QUERY_STATE, DYNAMIC_POOLS, POOLS};

/// Spawn a background task that periodically removes idle dynamic pools.
/// Dynamic pools are created by auth_query passthrough mode — one per user.
/// When all connections in a dynamic pool are closed (size == 0), the pool
/// is garbage-collected to prevent unbounded memory growth.
///
/// This is a no-op when DYNAMIC_POOLS is empty (no passthrough auth_query).
pub fn spawn_dynamic_pool_gc(interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip - runtime stalls should not trigger a burst
        // of GC sweeps that all race against `from_config` reloads.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            gc_idle_dynamic_pools();
        }
    });
}

fn gc_idle_dynamic_pools() {
    let dynamic_ids: Vec<PoolIdentifier> = DYNAMIC_POOLS.load().iter().cloned().collect();
    if dynamic_ids.is_empty() {
        return;
    }

    let pools = POOLS.load();
    let mut to_remove = Vec::new();

    for id in &dynamic_ids {
        match pools.get(id) {
            Some(pool) => {
                if !dynamic_pool_idle_for_gc(pool, id) {
                    continue;
                }
                debug!("[{id}] GC: 0 connections, marking for removal");
                to_remove.push(id.clone());
            }
            None => {
                debug!("[{id}] GC: stale entry (not in POOLS), removing");
                to_remove.push(id.clone());
            }
        }
    }

    if to_remove.is_empty() {
        return;
    }

    // cross-store removal of POOLS + DYNAMIC_POOLS under the
    // write-serialisation lock. Without this, a concurrent
    // create_dynamic_pool / drop_dynamic_pool / RELOAD that loaded an
    // earlier snapshot would silently undo our removal (last-write-wins),
    // or republish a POOLS map that disagrees with our DYNAMIC_POOLS
    // membership.
    let _commit_guard = super::pool_write_lock();

    // re-validate each `to_remove` candidate AFTER
    // taking the write lock. Pre-fix the `size == 0` check at line ~39
    // ran against a pre-lock POOLS snapshot - a client checkout that
    // started between that snapshot and the lock would set `size = 1`
    // before our remove landed, orphaning a backend that connects to
    // a pool no longer in POOLS. With the post-lock recheck, any pool
    // that has acquired its first checkout in the meantime is kept.
    let post_lock_pools = POOLS.load();
    let to_remove_validated: Vec<_> = to_remove
        .iter()
        .filter(|id| match post_lock_pools.get(*id) {
            Some(pool) => dynamic_pool_idle_for_gc(pool, id),
            None => true, // already gone - safe to drop from DYNAMIC_POOLS
        })
        .cloned()
        .collect();
    if to_remove_validated.is_empty() {
        drop(_commit_guard);
        return;
    }

    let aq_states = AUTH_QUERY_STATE.load();
    for id in &to_remove_validated {
        if let Some(state) = aq_states.get(&id.db) {
            state
                .stats
                .dynamic_pools_destroyed
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut new_pools = (**post_lock_pools).clone();
    drop(post_lock_pools);
    let mut removed_pools = Vec::new();
    for id in &to_remove_validated {
        if let Some(pool) = new_pools.remove(id) {
            pool.database.close_new_checkouts();
            removed_pools.push(pool);
        }
    }
    POOLS.store(Arc::new(new_pools));

    let mut new_dynamic = (**DYNAMIC_POOLS.load()).clone();
    for id in &to_remove_validated {
        new_dynamic.remove(id);
    }
    DYNAMIC_POOLS.store(Arc::new(new_dynamic));
    drop(_commit_guard);

    for pool in &removed_pools {
        pool.database.close();
    }

    // Update `to_remove` for downstream logging so we don't claim to
    // have removed pools that survived the post-lock recheck.
    let to_remove = to_remove_validated;

    info!(
        "GC: removed {} idle dynamic pool(s): {}",
        to_remove.len(),
        to_remove
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn dynamic_pool_status_idle_for_gc(status: &Status) -> bool {
    status.size == 0 && status.waiting == 0
}

fn dynamic_pool_idle_for_gc(pool: &ConnectionPool, id: &PoolIdentifier) -> bool {
    let status = pool.pool_state();
    if !dynamic_pool_status_idle_for_gc(&status) {
        if status.size != 0 {
            return false;
        }
        debug!(
            "[{id}] GC: {} checkout(s) in progress, skipping",
            status.waiting
        );
        return false;
    }
    should_gc_idle_pool(pool, id)
}

/// Decide whether an idle dynamic pool (`pool_state().size == 0`, no waiters) is
/// eligible for removal during this GC sweep. A pool is kept when it is
/// paused (admin control), when it has a `min_pool_size` (retain cycle
/// is responsible), when its first server connection is still being
/// established (`init_complete == false`), or when another owner still
/// holds this pool generation (for example an authenticated idle client
/// cached the `ConnectionPool` after startup). The init case is the race
/// fix for issue #209 - `PoolInitGuard::commit` is what flips the flag
/// to `true` after `get_server_parameters` succeeds, and any guard
/// dropped without `commit` has already removed the pool entry by the
/// time the next sweep observes the map.
fn should_gc_idle_pool(pool: &ConnectionPool, id: &PoolIdentifier) -> bool {
    if pool.database.is_paused() {
        debug!("[{id}] GC: paused, skipping");
        return false;
    }
    if pool.settings.user.min_pool_size.unwrap_or(0) > 0 {
        debug!("[{id}] GC: min_pool_size > 0, skipping despite size=0");
        return false;
    }
    if !pool.init_complete.load(Ordering::Acquire) {
        debug!("[{id}] GC: init not complete, skipping");
        return false;
    }
    if Arc::strong_count(&pool.init_complete) > 1 {
        debug!("[{id}] GC: pool generation is still referenced, skipping");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Address, PoolMode, User};
    use crate::pool::{CheckQueryCache, Pool, PoolSettings, ServerPool};
    use dashmap::DashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    fn pool(init_complete: bool, min_pool_size: u32) -> ConnectionPool {
        let server_pool = ServerPool::new(
            Address::default(),
            User {
                min_pool_size: if min_pool_size == 0 {
                    None
                } else {
                    Some(min_pool_size)
                },
                ..User::default()
            },
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
            Duration::from_secs(5),
            Duration::from_secs(5),
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
                user: User {
                    min_pool_size: if min_pool_size == 0 {
                        None
                    } else {
                        Some(min_pool_size)
                    },
                    ..User::default()
                },
                db: "test_db".to_string(),
                idle_timeout_ms: 60_000,
                life_time_ms: 60_000,
                sync_server_parameters: false,
                intercept_discard_all: true,
                min_guaranteed_pool_size: 0,
            },
            config_hash: 0,
            per_user_startup_overlay_hash: crate::pool::empty_overlay_hash(),
            prepared_statement_cache: None,
            check_query_cache: Arc::new(CheckQueryCache::new()),
            coordinator: None,
            replenish_failures: Arc::new(AtomicU32::new(0)),
            init_complete: Arc::new(AtomicBool::new(init_complete)),
        }
    }

    #[test]
    fn idle_pool_with_completed_init_is_collected() {
        // Regular case: pool finished initializing, drained to zero, GC should reap it.
        let id = PoolIdentifier::new("test_db", "test_user");
        let p = pool(true, 0);
        assert!(should_gc_idle_pool(&p, &id));
    }

    #[test]
    fn idle_pool_with_cached_client_ref_is_skipped() {
        let id = PoolIdentifier::new("test_db", "test_user");
        let p = pool(true, 0);
        let _idle_client_cached_pool = p.clone();

        assert!(
            !should_gc_idle_pool(&p, &id),
            "GC must not remove a dynamic pool while an authenticated idle client \
             still holds a cached ConnectionPool clone"
        );
    }

    #[test]
    fn dynamic_pool_status_with_waiting_checkout_is_not_idle_for_gc() {
        assert!(dynamic_pool_status_idle_for_gc(&Status {
            max_size: 10,
            size: 0,
            available: 0,
            waiting: 0,
        }));
        assert!(
            !dynamic_pool_status_idle_for_gc(&Status {
                max_size: 10,
                size: 0,
                available: 0,
                waiting: 1,
            }),
            "GC must not remove a dynamic pool while a checkout is already in progress"
        );
    }

    #[test]
    fn dynamic_pool_gc_post_lock_recheck_uses_waiting_guard() {
        let src = include_str!("gc.rs");
        let post_lock_idx = src
            .find("let post_lock_pools = POOLS.load()")
            .expect("post-lock recheck not found");
        let post_lock = &src[post_lock_idx..];
        let validation_end = post_lock
            .find("let aq_states = AUTH_QUERY_STATE.load()")
            .expect("post-lock validation end not found");
        let validation = &post_lock[..validation_end];

        assert!(
            validation.contains("dynamic_pool_idle_for_gc(pool, id)"),
            "post-lock GC recheck must include waiting/in-flight checkout guard"
        );
        assert!(
            !validation.contains("pool.pool_state().size == 0"),
            "post-lock GC recheck must not regress to size-only eligibility"
        );
    }

    #[test]
    fn dynamic_pool_gc_closes_removed_generations_before_publish() {
        let src = include_str!("gc.rs");
        let post_lock_idx = src
            .find("let post_lock_pools = POOLS.load()")
            .expect("post-lock section not found");
        let post_lock = &src[post_lock_idx..];
        let store_idx = post_lock
            .find("POOLS.store")
            .expect("GC must publish POOLS after removal");
        let before_store = &post_lock[..store_idx];

        assert!(
            before_store.contains("close_new_checkouts"),
            "GC must close removed dynamic generations before publishing a POOLS map without them"
        );
    }

    #[test]
    fn idle_pool_still_initializing_is_skipped() {
        // Issue #209: GC must not reap a pool whose first server connection
        // is still being established. Without this check the next login
        // observes "No pool configured" and the connection is dropped.
        let id = PoolIdentifier::new("test_db", "test_user");
        let p = pool(false, 0);
        assert!(!should_gc_idle_pool(&p, &id));
    }

    #[test]
    fn pool_with_min_pool_size_is_skipped() {
        // The retain cycle keeps `min_pool_size` connections warm; GC must
        // never compete with it on the same pool.
        let id = PoolIdentifier::new("test_db", "test_user");
        let p = pool(true, 5);
        assert!(!should_gc_idle_pool(&p, &id));
    }

    #[test]
    fn flipping_init_complete_makes_pool_eligible() {
        // Same pool object, different observable behavior before and after
        // `PoolInitGuard::commit` runs. Concretizes the contract between the
        // guard and the GC sweep.
        let id = PoolIdentifier::new("test_db", "test_user");
        let p = pool(false, 0);
        assert!(!should_gc_idle_pool(&p, &id));
        p.init_complete.store(true, Ordering::Release);
        assert!(should_gc_idle_pool(&p, &id));
    }

    #[test]
    fn destroyed_counter_is_after_post_lock_validation() {
        let source = include_str!("gc.rs");
        let counter = source
            .find(concat!("dynamic_pools_", "destroyed"))
            .expect("counter increment present");
        let validated_empty = source
            .find("to_remove_validated.is_empty()")
            .expect("post-lock validation guard present");
        assert!(
            counter > validated_empty,
            "destroyed counter must only count candidates that survive the post-lock recheck"
        );
    }
}
