use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use log::{info, warn};
use rand::seq::SliceRandom;

use crate::config::get_config;
use crate::utils::{format_duration_ms, format_elapsed};

use super::{get_all_pools, get_retired_pools, release_unreferenced_retired_pools, ConnectionPool};

impl ConnectionPool {
    /// Retain pool connections based on idle timeout and lifetime settings.
    /// Returns the number of connections closed.
    /// If `max` is 0, all expired connections will be closed (unlimited).
    /// If `max` > 0, at most `max` connections will be closed across all pools,
    /// prioritizing the oldest connections first.
    ///
    /// Pools under client pressure are skipped: closing an idle connection
    /// the moment a client is queued behind it just turns a free recycle
    /// into a fresh connect on the wait path.
    pub fn retain_pool_connections(&self, count: Arc<AtomicUsize>, max: usize) -> usize {
        if self.database.under_pressure() {
            return 0;
        }

        // Closure to determine if a connection should be closed
        // Uses per-connection timeouts with jitter to prevent mass closures
        let should_close = |_: &crate::server::Server, metrics: &crate::pool::Metrics| -> bool {
            // Check idle timeout (per-connection with jitter, 0 = disabled)
            if metrics.idle_timeout_ms > 0 {
                if let Some(v) = metrics.recycled {
                    if (v.elapsed().as_millis() as u64) > metrics.idle_timeout_ms {
                        return true;
                    }
                }
            }
            // Check server lifetime (per-connection with jitter, 0 = disabled)
            if metrics.lifetime_ms > 0 && (metrics.age().as_millis() as u64) > metrics.lifetime_ms {
                return true;
            }
            false
        };

        // Calculate remaining quota for this pool
        let current_count = count.load(Ordering::Relaxed);
        if max > 0 && current_count >= max {
            return 0; // Quota exhausted, skip this pool
        }
        let max_to_close = if max > 0 {
            max - current_count
        } else {
            0 // 0 means unlimited
        };

        // Use retain_oldest_first which sorts by age when max > 0
        let closed = self
            .database
            .retain_oldest_first(should_close, max_to_close);
        count.fetch_add(closed, Ordering::Relaxed);

        if closed > 0 {
            let idle_timeout = self.settings.idle_timeout_ms;
            let lifetime = self.settings.life_time_ms;
            let limits = match (idle_timeout > 0, lifetime > 0) {
                (true, true) => format!(
                    "idle_timeout=~{}, lifetime=~{}",
                    format_duration_ms(idle_timeout),
                    format_duration_ms(lifetime),
                ),
                (true, false) => format!("idle_timeout=~{}", format_duration_ms(idle_timeout)),
                (false, true) => format!("lifetime=~{}", format_duration_ms(lifetime)),
                (false, false) => "no limits configured".to_string(),
            };
            info!(
                "[{}@{}] closed {} idle server{}: expired ({})",
                self.address.username,
                self.address.pool_name,
                closed,
                if closed == 1 { "" } else { "s" },
                limits,
            );
        }

        closed
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
                "[{}@{}] drained {} idle server{}",
                self.address.username,
                self.address.pool_name,
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
    let dead_check_timeout = config.general.dead_backend_check_timeout.as_std();
    let dead_check_max = config.general.dead_backend_check_max_per_cycle;
    let mut interval = tokio::time::interval(retain_time);
    // Skip - after a runtime stall (eviction storm, paging),
    // the retain loop should not fire all backlog ticks at once and
    // re-acquire every pool's slots lock back-to-back.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Period the `interval` ticker was built with, so a live
    // retain_connections_time change can rebuild it.
    let mut current_retain_time = retain_time;
    let count = Arc::new(AtomicUsize::new(0));

    info!(
        "Retain task started: interval={}, max_per_cycle={}",
        format_elapsed(retain_time),
        if retain_max == 0 {
            "unlimited".to_string()
        } else {
            retain_max.to_string()
        }
    );
    if dead_check_timeout.is_zero() || dead_check_max == 0 {
        info!("Dead-backend liveness scan: disabled");
    } else {
        info!(
            "Dead-backend liveness scan: enabled, per-backend timeout={}, max_per_pool_per_cycle={}",
            format_elapsed(dead_check_timeout),
            dead_check_max,
        );
    }

    // Prewarm pools with min_pool_size before the first retain cycle
    for (_, pool) in get_all_pools().iter() {
        // drift from the steady-state replenish path
        // (which skips paused pools). If a pool starts paused (e.g.
        // admin issued PAUSE between `from_config` Ok and retain
        // task start), the initial prewarm bypassed the
        // pause and created backends regardless.
        if pool.database.is_paused() {
            continue;
        }
        if let Some(min_pool_size) = pool.settings.user.min_pool_size {
            let min = min_pool_size as usize;
            let created = pool.database.replenish(min).await;
            if created > 0 {
                info!(
                    "[{}@{}] prewarmed {} server{} (min_pool_size={})",
                    pool.address.username,
                    pool.address.pool_name,
                    created,
                    if created == 1 { "" } else { "s" },
                    min,
                );
            } else {
                warn!(
                    "[{}@{}] prewarm failed (min_pool_size={})",
                    pool.address.username, pool.address.pool_name, min,
                );
            }
        }
    }

    loop {
        interval.tick().await;

        // Use a single snapshot for both retain and replenish phases
        // to avoid inconsistency if POOLS is atomically updated between them.
        let pools = get_all_pools();

        // Shuffle pool iteration order for fair retain_connections_max distribution.
        // HashMap iteration order is deterministic within a process (fixed RandomState seed),
        // so without shuffling the same pool always gets the entire quota.
        let mut pool_refs: Vec<_> = pools.values().collect();
        pool_refs.shuffle(&mut rand::rng());

        // Reserve pressure relief runs in two steps, both off the hot path.
        //
        // Step 1 — upgrade: if any backend in this pool still holds a
        // reserve permit while the coordinator's main semaphore has
        // headroom, swap the accounting so the reserve slot is freed
        // without closing the backend. This fixes the case where a past
        // burst left reserve permits pinned to idle backends, making
        // `reserve_used` misrepresent actual burst buffer availability.
        //
        // Step 2 — close stale: for reserve backends that could not be
        // upgraded (main is still full) AND have been idle longer than
        // `min_connection_lifetime_ms`, close them the old way. These
        // two steps together guarantee that `reserve_used` converges to
        // the number of reserve permits actually defending against live
        // pressure, not to the number of grants.
        //
        // Pools currently under client pressure are skipped: closing a
        // reserve connection in front of a queued client just forces a
        // connect() onto the wait path. Reserve cleanup runs again next
        // cycle.
        for pool in &pool_refs {
            if pool.database.under_pressure() {
                continue;
            }
            if let Some(ref coordinator) = pool.coordinator {
                let upgraded = pool.database.upgrade_reserve_to_main();
                if upgraded > 0 {
                    info!(
                        "[{}@{}] upgraded {} reserve permit{} to main \
                         (main has headroom)",
                        pool.address.username,
                        pool.address.pool_name,
                        upgraded,
                        if upgraded == 1 { "" } else { "s" },
                    );
                }
                let min_lifetime = coordinator.config().min_connection_lifetime_ms;
                let closed = pool.database.close_idle_reserve_connections(min_lifetime);
                if closed > 0 {
                    info!(
                        "[{}@{}] released {} reserve server{} (idle > {})",
                        pool.address.username,
                        pool.address.pool_name,
                        closed,
                        if closed == 1 { "" } else { "s" },
                        format_duration_ms(min_lifetime),
                    );
                }
            }
        }

        // Dead-backend liveness scan. Runs BEFORE idle/lifetime trimming so
        // that a backend whose TCP socket died (e.g. PostgreSQL restart) is
        // dropped here and `slots.size` reflects reality by the time the
        // replenish phase below decides whether `current_size < min`.
        // Skipped per-pool when disabled, under_pressure, or paused - see
        // `Pool::evict_dead_backends` for the gating.
        //
        // Re-fetch the retain / dead-check knobs from the current config on
        // every tick so a SIGHUP / admin RELOAD takes effect without a process
        // restart. When retain_connections_time changes, rebuild the interval
        // ticker; interval_at starts the next tick a full period out so the
        // reload does not fire an extra immediate tick. A reload to zero is
        // ignored to keep tokio::time::interval from panicking.
        let live_cfg = get_config();
        let dead_check_timeout = live_cfg.general.dead_backend_check_timeout.as_std();
        let dead_check_max = live_cfg.general.dead_backend_check_max_per_cycle;
        let retain_time = live_cfg.general.retain_connections_time.as_std();
        let retain_max = live_cfg.general.retain_connections_max;
        if retain_time != current_retain_time && !retain_time.is_zero() {
            info!(
                "Retain interval updated {} -> {}",
                format_elapsed(current_retain_time),
                format_elapsed(retain_time)
            );
            current_retain_time = retain_time;
            interval =
                tokio::time::interval_at(tokio::time::Instant::now() + retain_time, retain_time);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        }

        // Pools are probed concurrently with a small fan-out cap so a
        // mass-PG-outage (every pool's backends timing out simultaneously)
        // collapses what would otherwise be `N_pools × max_per_cycle ×
        // timeout` of serial wall-clock - which on a 10+ pool deployment
        // with the defaults (8 × 2s = 16s per pool) easily exceeds the
        // 30s `retain_connections_time` tick. The cap keeps the dead-check
        // from monopolising the runtime when N is large; bounded
        // concurrency of 8 is enough to stay under one tick for typical
        // deployments while leaving headroom for the replenish phase
        // below.
        if !dead_check_timeout.is_zero() && dead_check_max > 0 {
            use futures::stream::{self, StreamExt};
            const SCAN_CONCURRENCY: usize = 8;
            stream::iter(pool_refs.iter())
                .for_each_concurrent(SCAN_CONCURRENCY, |pool| async move {
                    let (checked, evicted) = pool
                        .database
                        .evict_dead_backends(dead_check_timeout, dead_check_max, retain_time)
                        .await;
                    if evicted > 0 {
                        info!(
                            "[{}@{}] evicted {} dead backend{} (checked {} idle)",
                            pool.address.username,
                            pool.address.pool_name,
                            evicted,
                            if evicted == 1 { "" } else { "s" },
                            checked,
                        );
                    }
                })
                .await;
        }

        // Idle / lifetime trimming. Pools under client pressure are skipped
        // inside `retain_pool_connections` itself.
        for pool in &pool_refs {
            pool.retain_pool_connections(count.clone(), retain_max);
        }
        count.store(0, Ordering::Relaxed);

        // Replenish pools below min_pool_size
        for pool in &pool_refs {
            // Don't replenish paused pools — no new connections during PAUSE
            if pool.database.is_paused() {
                continue;
            }
            if let Some(min_pool_size) = pool.settings.user.min_pool_size {
                let min = min_pool_size as usize;
                let current_size = pool.database.status().size;
                if current_size < min {
                    let deficit = min - current_size;
                    let created = pool.database.replenish(deficit).await;
                    if created > 0 {
                        let prev_failures = pool.replenish_failures.swap(0, Ordering::Relaxed);
                        if prev_failures > 0 {
                            info!(
                                "[{}@{}] replenish recovered after {} failure{}: created {} server{} (min_pool_size={})",
                                pool.address.username, pool.address.pool_name,
                                prev_failures,
                                if prev_failures == 1 { "" } else { "s" },
                                created,
                                if created == 1 { "" } else { "s" },
                                min,
                            );
                        } else {
                            info!(
                                "[{}@{}] replenished {} server{} (min_pool_size={})",
                                pool.address.username,
                                pool.address.pool_name,
                                created,
                                if created == 1 { "" } else { "s" },
                                min,
                            );
                        }
                    } else {
                        let failures = pool.replenish_failures.fetch_add(1, Ordering::Relaxed) + 1;
                        if failures == 1 {
                            warn!(
                                "[{}@{}] replenish failed (deficit={}, min_pool_size={})",
                                pool.address.username, pool.address.pool_name, deficit, min,
                            );
                        } else if failures % 20 == 0 {
                            warn!(
                                "[{}@{}] replenish still failing: {} consecutive failures (deficit={}, min_pool_size={})",
                                pool.address.username, pool.address.pool_name,
                                failures, deficit, min,
                            );
                        }
                    }
                }
            }
        }

        // Generations that a RELOAD replaced are not in `POOLS`, so every
        // phase above skipped them — while their semaphores stay open so
        // they keep serving (and creating) backends for the sessions that
        // still hold them. Sweep them last: live pools have priority, and
        // the retired ones only need to shrink and eventually disappear.
        housekeep_retired_generations(dead_check_timeout, dead_check_max, retain_time, retain_max)
            .await;
    }
}

/// What one retired-generation housekeeping pass did.
///
/// Returned (and logged) so the contract is observable: a RELOAD-replaced
/// generation is visited by the same trim and dead-backend scan as a live
/// pool, and is released once no session holds it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RetiredSweepReport {
    /// Generations handed to the idle/lifetime trim.
    pub trim_visited: usize,
    /// Generations handed to the dead-backend liveness scan (0 when the
    /// scan is disabled by config).
    pub scan_visited: usize,
    /// Idle server connections closed by the trim.
    pub closed_connections: usize,
    /// Dead backends evicted by the scan.
    pub evicted_backends: usize,
    /// Generations closed and dropped from the registry.
    pub released_generations: usize,
}

/// Housekeeping for pool generations that a RELOAD replaced while sessions
/// were still holding them (see `pool::RETIRED_POOLS`).
///
/// Same trim and same dead-backend scan as a live pool, deliberately WITHOUT
/// replenish: a retired generation is unreachable for new sessions, so
/// refilling it to `min_pool_size` would recreate exactly the surplus
/// backends this sweep exists to remove. The pass ends by closing the
/// generations no session references any more.
pub(crate) async fn housekeep_retired_generations(
    dead_check_timeout: std::time::Duration,
    dead_check_max: usize,
    retain_time: std::time::Duration,
    retain_max: usize,
) -> RetiredSweepReport {
    let mut report = RetiredSweepReport::default();

    {
        let retired = get_retired_pools();
        if retired.is_empty() {
            return report;
        }

        // Dead-backend liveness scan, mirroring the live-pool phase
        // (including the bounded fan-out, so a mass-PG-outage cannot make
        // this exceed one retain tick).
        if !dead_check_timeout.is_zero() && dead_check_max > 0 {
            use futures::stream::{self, StreamExt};
            const SCAN_CONCURRENCY: usize = 8;
            let evicted_total = Arc::new(AtomicUsize::new(0));
            stream::iter(retired.iter())
                .for_each_concurrent(SCAN_CONCURRENCY, |pool| {
                    let evicted_total = Arc::clone(&evicted_total);
                    async move {
                        let (checked, evicted) = pool
                            .database
                            .evict_dead_backends(dead_check_timeout, dead_check_max, retain_time)
                            .await;
                        if evicted > 0 {
                            evicted_total.fetch_add(evicted, Ordering::Relaxed);
                            info!(
                                "[{}@{}] retired generation: evicted {} dead backend{} \
                                 (checked {} idle)",
                                pool.address.username,
                                pool.address.pool_name,
                                evicted,
                                if evicted == 1 { "" } else { "s" },
                                checked,
                            );
                        }
                    }
                })
                .await;
            report.scan_visited = retired.len();
            report.evicted_backends = evicted_total.load(Ordering::Relaxed);
        }

        // Idle / lifetime trimming. Retired generations get their own
        // `retain_connections_max` budget so they never eat into the quota
        // of the pools actually serving new sessions.
        let count = Arc::new(AtomicUsize::new(0));
        for pool in retired.iter() {
            report.closed_connections += pool.retain_pool_connections(count.clone(), retain_max);
            report.trim_visited += 1;
        }

        // No replenish phase here, on purpose — see the doc comment.
    } // The registry snapshot is scoped on purpose: entries are
      // `Arc<ConnectionPool>` so holding it cannot inflate the liveness
      // probe, but dropping it here lets a released generation be freed as
      // soon as it is closed instead of at the end of the sweep.

    report.released_generations = release_unreferenced_retired_pools();

    if report.closed_connections > 0
        || report.evicted_backends > 0
        || report.released_generations > 0
    {
        info!(
            "Retired generations swept: {} trimmed, {} scanned, {} idle server(s) closed, \
             {} dead backend(s) evicted, {} generation(s) released",
            report.trim_visited,
            report.scan_visited,
            report.closed_connections,
            report.evicted_backends,
            report.released_generations,
        );
    }

    report
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
            "Graceful shutdown: drained {} idle server{} from all pools",
            total_drained,
            if total_drained == 1 { "" } else { "s" }
        );
    }
    total_drained
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    use crate::config::{Address, PoolMode, User};
    use crate::pool::{Pool, PoolSettings, ServerPool};
    use dashmap::DashMap;

    fn build_test_connection_pool() -> ConnectionPool {
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
                user: User::default(),
                db: "test_db".to_string(),
                idle_timeout_ms: 60_000,
                life_time_ms: 1, // tiny: any connection would be "expired"
                sync_server_parameters: false,
                intercept_discard_all: true,
                min_guaranteed_pool_size: 0,
            },
            config_hash: 0,
            per_user_startup_overlay_hash: crate::pool::empty_overlay_hash(),
            prepared_statement_cache: None,
            check_query_cache: Arc::new(crate::pool::CheckQueryCache::new()),
            coordinator: None,
            replenish_failures: Arc::new(AtomicU32::new(0)),
            init_complete: Arc::new(AtomicBool::new(true)),
        }
    }

    /// A generation that a RELOAD replaced is no longer in `POOLS`, so every
    /// phase of the retain loop skips it. It must instead be reached through
    /// the retirement registry: same idle/lifetime trim, same dead-backend
    /// scan, and — once no session holds it any more — closed and dropped
    /// from the registry.
    #[tokio::test]
    #[serial_test::serial(retired_pools)]
    async fn retired_generation_is_swept_and_released() {
        crate::pool::clear_retired_pools_for_test();

        let pool = build_test_connection_pool();
        let database = pool.database.clone();
        crate::pool::retire_pool_generations(vec![pool]);

        let report = housekeep_retired_generations(
            Duration::from_millis(500),
            8,
            Duration::from_secs(30),
            0,
        )
        .await;

        assert_eq!(
            report.trim_visited, 1,
            "a retired generation must go through idle/lifetime trimming"
        );
        assert_eq!(
            report.scan_visited, 1,
            "a retired generation must go through the dead-backend scan"
        );
        assert_eq!(
            report.released_generations, 1,
            "an unreferenced retired generation must be released"
        );
        assert!(
            crate::pool::get_retired_pools().is_empty(),
            "the registry must not keep released generations"
        );
        assert!(
            database.is_closed(),
            "a released generation must be closed so its backends go away"
        );
    }

    /// The drain guarantee: while a session still holds the old generation,
    /// housekeeping visits it (so its idle backends expire) but must not
    /// close it — closing would fail the in-flight session with 53300.
    #[tokio::test]
    #[serial_test::serial(retired_pools)]
    async fn retired_generation_in_use_is_swept_but_kept_open() {
        crate::pool::clear_retired_pools_for_test();

        let pool = build_test_connection_pool();
        let database = pool.database.clone();
        let inflight_client = pool.clone();
        crate::pool::retire_pool_generations(vec![pool]);

        let report = housekeep_retired_generations(
            Duration::from_millis(500),
            8,
            Duration::from_secs(30),
            0,
        )
        .await;

        assert_eq!(report.trim_visited, 1);
        assert_eq!(report.scan_visited, 1);
        assert_eq!(
            report.released_generations, 0,
            "a generation an in-flight session still holds must not be released"
        );
        assert_eq!(crate::pool::get_retired_pools().len(), 1);
        assert!(
            !database.is_closed(),
            "housekeeping must never close a generation an in-flight session holds"
        );

        // Session disconnects → the next sweep reaps the generation.
        drop(inflight_client);
        let report = housekeep_retired_generations(
            Duration::from_millis(500),
            8,
            Duration::from_secs(30),
            0,
        )
        .await;
        assert_eq!(report.released_generations, 1);
        assert!(crate::pool::get_retired_pools().is_empty());
        assert!(database.is_closed());
    }

    /// The dead-backend scan honours the same operator kill switch on
    /// retired generations as on live pools.
    #[tokio::test]
    #[serial_test::serial(retired_pools)]
    async fn retired_generation_scan_respects_disabled_dead_check() {
        crate::pool::clear_retired_pools_for_test();

        let pool = build_test_connection_pool();
        let inflight_client = pool.clone();
        crate::pool::retire_pool_generations(vec![pool]);

        let report =
            housekeep_retired_generations(Duration::ZERO, 8, Duration::from_secs(30), 0).await;
        assert_eq!(
            report.scan_visited, 0,
            "dead_backend_check_timeout = 0 must disable the scan for retired generations too"
        );
        assert_eq!(
            report.trim_visited, 1,
            "the idle/lifetime trim is independent of the dead-backend scan"
        );

        drop(inflight_client);
        crate::pool::release_unreferenced_retired_pools();
    }

    /// An empty registry is the steady state; it must cost nothing and
    /// report nothing.
    #[tokio::test]
    #[serial_test::serial(retired_pools)]
    async fn empty_registry_sweep_is_a_no_op() {
        crate::pool::clear_retired_pools_for_test();
        let report = housekeep_retired_generations(
            Duration::from_millis(500),
            8,
            Duration::from_secs(30),
            0,
        )
        .await;
        assert_eq!(report, RetiredSweepReport::default());
    }

    /// Retired generations must never be replenished: refilling a pool that
    /// no new session can reach is exactly the backend duplication this
    /// registry exists to remove.
    #[test]
    fn retired_housekeeping_never_replenishes() {
        let src = include_str!("retain.rs");
        let start = src
            .find("pub(crate) async fn housekeep_retired_generations")
            .expect("retired housekeeping entry point must exist");
        let end = src[start..]
            .find("\n/// Drain all idle connections from all pools")
            .map(|offset| start + offset)
            .expect("retired housekeeping tail marker must exist");
        let body = &src[start..end];

        assert!(
            !body.contains(".replenish("),
            "retired generations must not be replenished"
        );
        assert!(
            body.contains("retain_pool_connections"),
            "retired generations must go through idle/lifetime trimming"
        );
        assert!(
            body.contains("evict_dead_backends"),
            "retired generations must go through the dead-backend scan"
        );
        assert!(
            body.contains("release_unreferenced_retired_pools"),
            "the sweep must release generations nobody holds any more"
        );
    }

    /// The retain loop must actually call the retired-generation sweep;
    /// otherwise the registry grows without ever being housekept.
    #[test]
    fn retain_loop_sweeps_retired_generations() {
        let src = include_str!("retain.rs");
        let start = src
            .find("pub async fn retain_connections()")
            .expect("retain task entry point must exist");
        let end = src[start..]
            .find("\n/// Drain all idle connections from all pools")
            .map(|offset| start + offset)
            .expect("retain task tail marker must exist");
        let body = &src[start..end];

        assert!(
            body.contains("housekeep_retired_generations("),
            "the retain loop must sweep RELOAD-replaced generations, otherwise they \
             keep their backends open until process restart"
        );
    }

    /// Pools serving live traffic must not lose idle connections to retain
    /// trimming. The whole point of the skip is to make sure that an idle
    /// connection a queued client is about to grab is not closed for
    /// housekeeping reasons one tick before. Drain the semaphore (models
    /// "every permit is in flight"), call retain, and assert no closures.
    #[tokio::test]
    async fn retain_pool_skips_under_pressure() {
        let conn_pool = build_test_connection_pool();

        let semaphore = conn_pool.database.semaphore();
        let total_permits = semaphore.available_permits();
        let mut held = Vec::with_capacity(total_permits);
        for _ in 0..total_permits {
            held.push(semaphore.acquire().await.unwrap());
        }
        assert!(
            conn_pool.database.under_pressure(),
            "test setup must put the pool under pressure",
        );

        let count = Arc::new(AtomicUsize::new(0));
        let closed = conn_pool.retain_pool_connections(count.clone(), 0);

        assert_eq!(
            closed, 0,
            "retain must close zero connections under pressure"
        );
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "shared retain counter must not advance",
        );
    }
}
