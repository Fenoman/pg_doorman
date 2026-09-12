//! Pool eviction source for the coordinator.
//!
//! Bridges `PoolCoordinator`'s eviction callbacks to real pool state,
//! scanning idle connections across user pools for the same database.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use log::{debug, info};

use crate::utils::format_duration_ms;

use super::pool_coordinator;
use super::{get_pool, get_retired_pools, ConnectionPool, Pool, POOLS};

/// Adapter bridging `PoolCoordinator`'s eviction callbacks to real pool state.
///
/// The coordinator calls these methods when it needs to free a connection slot:
/// - `try_evict_one`: close one idle connection from another user's pool
/// - `queued_clients`: how many clients are waiting for this user's pool
/// - `is_starving`: whether a user is below their guaranteed minimum
pub struct PoolEvictionSource<'a> {
    database: &'a str,
    requesting_pool: &'a Pool,
    coordinator: &'a Arc<pool_coordinator::PoolCoordinator>,
}

impl<'a> PoolEvictionSource<'a> {
    pub fn new(
        database: &'a str,
        requesting_pool: &'a Pool,
        coordinator: &'a Arc<pool_coordinator::PoolCoordinator>,
    ) -> Self {
        Self {
            database,
            requesting_pool,
            coordinator,
        }
    }

    fn eligible_capacity(
        &self,
        user: &str,
        pool: &ConnectionPool,
        requesting_user: &str,
    ) -> Option<usize> {
        if pool.database.same_instance(self.requesting_pool)
            || !pool
                .coordinator
                .as_ref()
                .is_some_and(|coordinator| Arc::ptr_eq(coordinator, self.coordinator))
        {
            return None;
        }
        if user == requesting_user {
            // A replacement backend still belongs to the same logical user.
            // Applying its minimum independently to every generation would
            // strand the budget in whichever generation obtained it first.
            Some(pool.pool_state().available)
        } else {
            Some(pool.spare_above_min())
        }
    }
}

impl pool_coordinator::EvictionSource for PoolEvictionSource<'_> {
    /// Evict one idle connection from the user with the largest surplus.
    ///
    /// Scans live and retired pools sharing this exact coordinator, skipping
    /// the requesting physical pool. Same-user replacements share one budget.
    /// Snapshots `spare_above_min()` once per candidate to avoid TOCTOU
    /// inconsistency from repeated locking. Evicts only connections older
    /// than `min_connection_lifetime`. The evicted connection's
    /// `CoordinatorPermit` drops synchronously, freeing the slot.
    fn try_evict_one(&self, requesting_user: &str) -> bool {
        let all_pools = POOLS.load();
        let retired = get_retired_pools();

        // Snapshot spare count and p95 xact time once per candidate.
        // Spare avoids TOCTOU from repeated locking. p95 is an atomic
        // load (~3ns) from a value cached every 15s in the stats cycle.
        // collect the other-user pools once, then partition in
        // place by a single sort instead of building a second
        // `candidates` Vec via `.cloned()`. The spare>0 entries sort
        // ahead of spare==0 entries, so `candidates` is a borrowed prefix
        // of the same Vec - one allocation, identical victim ordering
        // (p95 desc, spare desc), and the spare==0 suffix stays available
        // for the diagnostic log below.
        let mut all_other_users: Vec<(&str, &ConnectionPool, usize, u64)> = all_pools
            .iter()
            .map(|(id, pool)| (id.db.as_str(), id.user.as_str(), pool))
            .chain(retired.iter().map(|pool| {
                (
                    pool.address.pool_name.as_str(),
                    pool.address.username.as_str(),
                    pool.as_ref(),
                )
            }))
            .filter(|(db, _, _)| *db == self.database)
            .filter_map(|(_, user, pool)| {
                let spare = self.eligible_capacity(user, pool, requesting_user)?;
                let p95 = pool.address.stats.p95_xact_time_us.load(Ordering::Relaxed);
                Some((user, pool, spare, p95))
            })
            .collect();

        // Slow pools (high p95 xact time) donate first - they tolerate
        // the re-create cost better. 1ms of pool wait adds 6.7% to a
        // 15ms p95 but 104% to a 0.96ms p95. Spare count as tiebreaker
        // when p95 is equal or not yet computed (0). spare==0 entries are
        // pushed to the tail so the spare>0 prefix is the candidate set.
        all_other_users.sort_by(|a, b| evict_candidate_order((a.2, a.3), (b.2, b.3)));
        let candidate_count = all_other_users.iter().filter(|(_, _, s, _)| *s > 0).count();
        let candidates = &all_other_users[..candidate_count];

        if candidates.is_empty() {
            if all_other_users.is_empty() {
                debug!(
                    "[{requesting_user}@{}] eviction: no peer pool generations share this coordinator",
                    self.database,
                );
            } else {
                debug!(
                    "[{requesting_user}@{}] eviction: {} peer pool generation(s) checked, none have \
                     eligible connections above guaranteed minimum (users: {})",
                    self.database,
                    all_other_users.len(),
                    all_other_users
                        .iter()
                        .map(|(id, _, spare, p95)| format!("{id}(spare={spare}, p95_xact={p95}us)"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            return false;
        }

        debug!(
            "[{requesting_user}@{}] eviction: {} candidate(s) with spare connections ({})",
            self.database,
            candidates.len(),
            candidates
                .iter()
                .map(|(id, _, spare, p95)| format!("{id}(spare={spare}, p95_xact={p95}us)"))
                .collect::<Vec<_>>()
                .join(", "),
        );

        let min_lifetime_ms = self.coordinator.config().min_connection_lifetime_ms;

        for (id, pool, spare, _) in candidates {
            // Re-check spare to narrow TOCTOU window: another thread may have
            // acquired a connection since the snapshot, reducing spare to 0.
            let current_spare = self
                .eligible_capacity(id, pool, requesting_user)
                .unwrap_or(0);
            if current_spare == 0 {
                debug!(
                    "[{}@{}] eviction: skipped — spare dropped to 0 since snapshot \
                     (was {}, requesting_user='{}')",
                    id, self.database, spare, requesting_user,
                );
                continue;
            }
            if pool.database.evict_one_idle(min_lifetime_ms) {
                info!(
                    "[{}@{}] coordinator evicted idle connection \
                     (spare={}, min_lifetime={}) to free slot for '{}'",
                    id,
                    self.database,
                    spare,
                    format_duration_ms(min_lifetime_ms),
                    requesting_user,
                );
                return true;
            }
            debug!(
                "[{}@{}] eviction: candidate skipped — \
                 no idle connections older than {} (spare={})",
                id,
                self.database,
                format_duration_ms(min_lifetime_ms),
                spare,
            );
        }

        debug!(
            "[{requesting_user}@{}] eviction: all {} candidate(s) had connections \
             too young to evict (min_lifetime={})",
            self.database,
            candidates.len(),
            format_duration_ms(min_lifetime_ms),
        );
        false
    }

    fn queued_clients(&self, user: &str) -> usize {
        get_pool(self.database, user)
            .map(|p| p.pool_state().waiting)
            .unwrap_or(0)
    }

    fn is_starving(&self, user: &str) -> bool {
        get_pool(self.database, user)
            .map(|p| {
                let user_min = p.settings.user.min_pool_size.unwrap_or(0) as usize;
                let pool_min = p.settings.min_guaranteed_pool_size as usize;
                let effective_min = user_min.max(pool_min);
                let current = p.pool_state().size;
                current < effective_min
            })
            .unwrap_or(false)
    }
}

/// total ordering for eviction victim selection. Entries with
/// spare connections (spare > 0) sort before spare==0 entries; within the
/// kept (spare>0) group the order is p95 desc then spare desc - slow
/// pools donate first, larger surplus breaks ties. A named function lets
/// the production `sort_by` and the behavior-identity test share the exact
/// same comparator. Input tuples are `(spare, p95)`.
fn evict_candidate_order(a: (usize, u64), b: (usize, u64)) -> std::cmp::Ordering {
    let a_has = a.0 > 0;
    let b_has = b.0 > 0;
    b_has
        .cmp(&a_has)
        .then_with(|| b.1.cmp(&a.1))
        .then_with(|| b.0.cmp(&a.0))
}

#[cfg(test)]
mod tests {
    use super::evict_candidate_order;

    /// Behavior identity: the single in-place sort must
    /// produce the same kept-candidate (spare>0) ordering as the previous
    /// "filter spare>0, then sort by p95 desc / spare desc" two-step.
    #[test]
    fn single_sort_matches_filter_then_sort_ordering() {
        // (spare, p95) pairs, deliberately interleaving spare==0 entries.
        let input: Vec<(usize, u64)> = vec![
            (0, 9999), // no spare, high p95 - must trail despite p95
            (2, 100),
            (5, 100),  // same p95, more spare -> ranks ahead of (2,100)
            (1, 5000), // highest p95 among spare>0 -> ranks first overall
            (0, 0),
            (3, 50),
        ];

        // Reference: old two-step behavior.
        let mut reference: Vec<(usize, u64)> =
            input.iter().copied().filter(|(s, _)| *s > 0).collect();
        reference.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));

        // New: single sort of the full list, then take the spare>0 prefix.
        let mut full = input.clone();
        full.sort_by(|a, b| evict_candidate_order(*a, *b));
        let count = full.iter().filter(|(s, _)| *s > 0).count();
        let candidates = &full[..count];

        assert_eq!(candidates, reference.as_slice());
        // The spare==0 entries are all pushed to the tail.
        assert!(full[count..].iter().all(|(s, _)| *s == 0));
    }

    #[test]
    fn p95_dominates_spare_in_ordering() {
        // Slow pool (higher p95) donates first even with less spare.
        let slow_small = (1usize, 5000u64);
        let fast_big = (10usize, 10u64);
        assert_eq!(
            evict_candidate_order(slow_small, fast_big),
            std::cmp::Ordering::Less,
            "higher p95 must sort earlier (Less) regardless of spare"
        );
    }

    #[test]
    fn spare_breaks_p95_ties() {
        let a = (2usize, 100u64);
        let b = (5usize, 100u64);
        // Equal p95: more spare sorts earlier.
        assert_eq!(evict_candidate_order(b, a), std::cmp::Ordering::Less);
    }

    #[test]
    fn spare_zero_always_trails_spare_positive() {
        // A spare==0 pool never outranks a spare>0 pool, even with much
        // higher p95.
        let no_spare_slow = (0usize, 100_000u64);
        let some_spare_fast = (1usize, 1u64);
        assert_eq!(
            evict_candidate_order(no_spare_slow, some_spare_fast),
            std::cmp::Ordering::Greater
        );
    }
}
