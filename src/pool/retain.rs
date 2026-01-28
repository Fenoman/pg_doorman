use log::{info, warn};
use std::cmp::Reverse;
use std::time::Duration;

use crate::config::get_config;

use super::{get_all_pools, ConnectionPool};
use super::APPLICATION_NAME_SUFFIX;

impl ConnectionPool {
    pub fn retain_pool_connections(&self, max: usize) {
        let min_pool_size = self.settings.min_pool_size;
        let idle_timeout_ms = self.settings.idle_timeout_ms;
        let life_time_ms = self.settings.life_time_ms;
        let mut remaining = max;
        if remaining == 0 {
            return;
        }

        // Hard rotation for server_lifetime: ignore min_pool_size for expired connections.
        // Use retain_sorted to remove youngest connections first, keeping oldest ones.
        // This preserves connections that have accumulated state (like temp tables).
        // Sort key is age: younger connections have smaller age and will be removed first.
        let lifetime_removed = if life_time_ms > 0 {
            self.database.retain_sorted(
                |_, metrics| (metrics.age().as_millis() as u64) > life_time_ms,
                |metrics| Reverse(metrics.age()),
                0,
                remaining,
            )
        } else {
            0
        };

        if lifetime_removed > 0 {
            info!(
                "[pool: {}][user: {}] rotated {} connection(s) due to server_lifetime (>{}ms)",
                self.address.pool_name, self.address.username, lifetime_removed, life_time_ms
            );
        }

        remaining = remaining.saturating_sub(lifetime_removed);
        if remaining == 0 {
            return;
        }

        // Idle timeout rotation keeps min_pool_size connections.
        let idle_removed = if idle_timeout_ms > 0 {
            self.database.retain_sorted(
                |_, metrics| {
                    metrics
                        .recycled
                        .map(|v| (v.elapsed().as_millis() as u64) > idle_timeout_ms)
                        .unwrap_or(false)
                },
                |metrics| metrics.age(),
                min_pool_size,
                remaining,
            )
        } else {
            0
        };

        if idle_removed > 0 {
            info!(
                "[pool: {}][user: {}] rotated {} connection(s) due to idle_timeout (>{}ms, min_pool_size={})",
                self.address.pool_name,
                self.address.username,
                idle_removed,
                idle_timeout_ms,
                min_pool_size
            );
        }

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
        // Note: server creation is already serialized inside `ServerPool::create()`,
        // so spawning many tasks here only adds overhead and memory usage.
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

    /// Drain all idle connections from the pool during graceful shutdown.
    /// This immediately closes all idle connections and marks remaining ones for removal.
    pub fn drain_idle_connections(&self) -> usize {
        let status_before = self.database.status();
        let idle_before = status_before.available;

        // Close all idle connections by returning false for all
        self.database.retain(|_, _| false);

        let status_after = self.database.status();
        let closed = idle_before.saturating_sub(status_after.available);

        if closed > 0 {
            info!(
                "[pool: {}][user: {}] drained {} idle connection{}",
                self.address.pool_name,
                self.address.username,
                closed,
                if closed == 1 { "" } else { "s" }
            );
        }

        closed
    }
}

pub async fn retain_connections() {
    let retain_time = get_config().general.retain_connections_time.as_std();
    let mut interval = tokio::time::interval(retain_time);
    loop {
        interval.tick().await;
        for (_, pool) in get_all_pools().iter() {
            pool.retain_pool_connections(20);
        }
    }
}

/// Pre-warm all pools that have min_pool_size configured.
/// This should be called after pool creation to ensure minimum connections are ready.
pub async fn warm_up_pools() {
    let pools = get_all_pools();
    let mut handles = Vec::new();
    for (_, pool) in pools.iter() {
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
        for (id, pool) in get_all_pools().iter() {
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

/// Drain all idle connections from all pools during graceful shutdown.
/// Returns the total number of connections drained.
pub fn drain_all_pools() -> usize {
    let mut total_drained = 0;
    for (_, pool) in get_all_pools().iter() {
        total_drained += pool.drain_idle_connections();
    }
    if total_drained > 0 {
        info!(
            "Graceful shutdown: drained {} idle connection{} from all pools",
            total_drained,
            if total_drained == 1 { "" } else { "s" }
        );
    }
    total_drained
}
