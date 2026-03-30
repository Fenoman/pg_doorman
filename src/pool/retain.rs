use std::cmp::Reverse;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use log::{info, warn};
use rand::seq::SliceRandom;

use crate::config::get_config;

use super::APPLICATION_NAME_SUFFIX;
use super::{get_all_pools, ConnectionPool};

impl ConnectionPool {
    /// Retain pool connections based on idle timeout and lifetime settings.
    /// Returns the number of connections closed.
    /// If `max` is 0, all expired connections will be closed (unlimited).
    pub fn retain_pool_connections(&self, count: Arc<AtomicUsize>, max: usize) -> usize {
        let min_pool_size = self.settings.min_pool_size;
        let idle_timeout_ms = self.settings.idle_timeout_ms;
        let life_time_ms = self.settings.life_time_ms;

        let current_count = count.load(Ordering::Relaxed);
        let mut remaining = if max > 0 {
            max.saturating_sub(current_count)
        } else {
            usize::MAX
        };
        if remaining == 0 {
            return 0;
        }

        let lifetime_removed = if life_time_ms > 0 {
            self.database.retain_sorted(
                |_, metrics| {
                    metrics.lifetime_ms > 0
                        && (metrics.age().as_millis() as u64) > metrics.lifetime_ms
                },
                |metrics| Reverse(metrics.age()),
                0,
                remaining,
            )
        } else {
            0
        };

        if lifetime_removed > 0 {
            info!(
                "[pool: {}][user: {}] rotated {} connection(s) due to server_lifetime (base={}ms±20%)",
                self.address.pool_name,
                self.address.username,
                lifetime_removed,
                life_time_ms
            );
        }

        remaining = remaining.saturating_sub(lifetime_removed);
        if remaining == 0 {
            count.fetch_add(lifetime_removed, Ordering::Relaxed);
            return lifetime_removed;
        }

        let idle_removed = if idle_timeout_ms > 0 {
            self.database.retain_sorted(
                |_, metrics| {
                    metrics.idle_timeout_ms > 0
                        && metrics
                            .recycled
                            .map(|v| (v.elapsed().as_millis() as u64) > metrics.idle_timeout_ms)
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
                "[pool: {}][user: {}] rotated {} connection(s) due to idle_timeout (base={}ms±20%, min_pool_size={})",
                self.address.pool_name,
                self.address.username,
                idle_removed,
                idle_timeout_ms,
                min_pool_size
            );
        }

        let removed = lifetime_removed.saturating_add(idle_removed);
        count.fetch_add(removed, Ordering::Relaxed);
        removed
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

        // Use replenish() instead of get() so warm-up really creates missing backends
        // even when the pool still has some idle connections available.
        let created = APPLICATION_NAME_SUFFIX
            .scope("warm_up".to_string(), async {
                self.database.replenish(connections_to_create).await
            })
            .await;

        if log_on_startup {
            if created < connections_to_create {
                warn!(
                    "[pool: {}][user: {}] Pre-warming completed: {} created, {} failed",
                    self.address.pool_name,
                    self.address.username,
                    created,
                    connections_to_create - created
                );
            } else if created > 0 {
                info!(
                    "[pool: {}][user: {}] Pre-warming completed: {} connections created",
                    self.address.pool_name, self.address.username, created
                );
            }
        }

        let size_after = self.pool_state().size;
        size_after.saturating_sub(size_before)
    }

    /// Drain all idle connections from the pool during graceful shutdown.
    /// This immediately closes all idle connections and marks remaining ones for removal.
    pub fn drain_idle_connections(&self) -> usize {
        let status_before = self.database.status();
        let idle_before = status_before.available;

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
    let config = get_config();
    let retain_time = config.general.retain_connections_time.as_std();
    let retain_max = config.general.retain_connections_max;
    let mut interval = tokio::time::interval(retain_time);
    let count = Arc::new(AtomicUsize::new(0));

    info!(
        "Starting connection retain task: interval={}ms, max_per_cycle={}",
        retain_time.as_millis(),
        if retain_max == 0 {
            "unlimited".to_string()
        } else {
            retain_max.to_string()
        }
    );

    loop {
        interval.tick().await;

        let pools = get_all_pools();
        let mut pool_refs: Vec<_> = pools.values().collect();
        pool_refs.shuffle(&mut rand::rng());

        for pool in &pool_refs {
            pool.retain_pool_connections(count.clone(), retain_max);
        }
        count.store(0, Ordering::Relaxed);

        for pool in &pool_refs {
            if pool.database.is_paused() {
                continue;
            }

            let min = pool.settings.min_pool_size;
            if min > 0 {
                let current_size = pool.database.status().size;
                if current_size < min {
                    let deficit = min - current_size;
                    let created = pool.database.replenish(deficit).await;
                    if created > 0 {
                        info!(
                            "[pool: {}][user: {}] replenished {} connection{} (min_pool_size: {})",
                            pool.address.pool_name,
                            pool.address.username,
                            created,
                            if created == 1 { "" } else { "s" },
                            min,
                        );
                    } else {
                        warn!(
                            "[pool: {}][user: {}] failed to replenish connections (deficit: {}, min_pool_size: {})",
                            pool.address.pool_name,
                            pool.address.username,
                            deficit,
                            min,
                        );
                    }
                }
            }
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
