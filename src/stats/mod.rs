/// Statistics and reporting system for the PostgreSQL connection pooler.
///
/// This module provides a comprehensive statistics tracking system that monitors
/// various aspects of the connection pooler's operation, including:
///
/// - Client connections and their activities
/// - Server connections and their performance
/// - Connection pool usage and efficiency
/// - Query and transaction metrics
/// - Network throughput
///
/// The statistics are collected in real-time and periodically processed to calculate
/// averages and other derived metrics. These statistics can be queried through
/// administrative commands like SHOW CLIENTS and SHOW SERVERS.
use arc_swap::ArcSwap;
use log::{info, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;

// Sub-modules for different statistics components
// -----------------------------------------------------------------------------
/// Statistics for connections grouped by address
pub mod address;
/// Statistics for auth_query cache and authentication
pub mod auth_query;
/// Statistics for client connections
pub mod client;
/// Connection counters (internal)
mod connections;
/// Statistics for connection pools
pub mod pool;
/// Utilities for printing statistics (internal)
pub mod print_all_stats;
/// Statistics for server connections
pub mod server;
/// Socket-related statistics (Linux only)
#[cfg(target_os = "linux")]
pub mod socket;

// Public exports for commonly used types and functions
// -----------------------------------------------------------------------------
use crate::stats::print_all_stats::print_all_stats;
pub use address::AddressStats;
pub use client::{ClientStats, PreparedCacheSnapshot};
pub use connections::{
    CANCEL_CONNECTION_COUNTER, PLAIN_CONNECTION_COUNTER, TLS_CONNECTION_COUNTER,
    TOTAL_CONNECTION_COUNTER,
};
pub use server::ServerStats;
#[cfg(target_os = "linux")]
pub use socket::{
    cached_socket_states_count, get_socket_states_count, spawn_socket_states_refresh,
};

// Type definitions and global state
// -----------------------------------------------------------------------------
/// Type alias for the client statistics lookup table.
/// Maps client IDs to their corresponding statistics objects.
///
/// snapshot type for `get_client_stats()` consumers (admin SHOW
/// commands, JSON API). The live registry is a `DashMap` so producer
/// (connect/disconnect) and consumer (collector tick / scrape) paths
/// operate on independent shards instead of serialising through one
/// global `RwLock`.
type ClientStatesLookup = HashMap<u64, Arc<ClientStats>>;

/// Type alias for the server statistics lookup table.
/// Maps server IDs to their corresponding statistics objects.
type ServerStatesLookup = HashMap<i32, Arc<ServerStats>>;

/// Global registry of client statistics.
///
/// This static variable maintains a thread-safe collection of all active client
/// connections and their associated statistics. It is used by the SHOW CLIENTS
/// administrative command to display information about connected clients.
///
/// earlier `RwLock<HashMap>`; every client connect/disconnect took
/// the global write lock. Under a connect storm (PG restart wave ->
/// thousands of reconnects) every event serialised through this one
/// lock, and the 15s stats collector held the read lock for the full
/// per-server update pass, blocking all new connects during that window.
/// Migrated to `DashMap` so the producer side is shard-local and the
/// collector iterates without blocking new connects.
// route the connect-storm-hottest DashMaps through the
// `utils::dashmap` helper. Default `DashMap::new()` sizes shards via
// `num_cpus::get() * 4` (next-pow-of-2) - under k8s with a 4-vCPU
// quota on a 96-core host that gives 512 shards and 512 lock objects
// (cache-line waste); on small VMs it can undershoot. The helper
// derives shard count from the configured worker_threads, matching
// the actual concurrency. At Lazy init the config isn't loaded yet,
// so we approximate via `num_cpus::get()` clamped to a sane range.
static CLIENT_STATS: Lazy<Arc<dashmap::DashMap<u64, Arc<ClientStats>>>> = Lazy::new(|| {
    let cpus = num_cpus::get().clamp(2, 16);
    Arc::new(crate::utils::dashmap::new_dashmap(cpus))
});

/// Global registry of server statistics.
///
/// This static variable maintains a thread-safe collection of all active server
/// connections and their associated statistics. It is used by the SHOW SERVERS
/// administrative command to display information about server connections.
static SERVER_STATS: Lazy<Arc<dashmap::DashMap<i32, Arc<ServerStats>>>> = Lazy::new(|| {
    let cpus = num_cpus::get().clamp(2, 16);
    Arc::new(crate::utils::dashmap::new_dashmap(cpus))
});

/// Global statistics reporter instance.
///
/// This static variable provides a thread-safe reference to the statistics reporter.
/// The reporter is responsible for registering and unregistering clients and servers
/// with the statistics system.
pub static REPORTER: Lazy<ArcSwap<Reporter>> =
    Lazy::new(|| ArcSwap::from_pointee(Reporter::default()));

/// Statistics collection period in milliseconds.
///
/// This value determines how frequently statistics are collected and averages are
/// calculated. The current value is 15 seconds (15000 milliseconds).
static STAT_PERIOD: u64 = 15000;

/// Statistics reporter for registering and unregistering statistics sources.
///
/// The Reporter is responsible for managing the lifecycle of statistics objects
/// in the global registries. It provides methods for registering new clients and
/// servers when they connect, and for removing them when they disconnect.
///
/// An instance of this reporter is given to each possible source of statistics,
/// such as clients, servers, and connection pools.
#[derive(Clone, Debug, Default)]
pub struct Reporter {}

impl Reporter {
    /// Registers client stats; duplicate ids are logged and ignored.
    fn client_register(&self, client_id: u64, stats: Arc<ClientStats>) {
        use dashmap::mapref::entry::Entry;
        match CLIENT_STATS.entry(client_id) {
            Entry::Occupied(_) => {
                warn!("[#c{client_id}] duplicate stats registration, skipping (likely migrated client id collision)");
            }
            Entry::Vacant(entry) => {
                entry.insert(stats);
            }
        }
    }

    fn client_disconnecting(&self, client_id: u64) {
        CLIENT_STATS.remove(&client_id);
    }

    fn server_register(&self, server_id: i32, stats: Arc<ServerStats>) {
        // mirror `client_register`'s duplicate-aware semantics.
        // server_id is `i32` (vs client_id `u64`) so collisions are
        // far more likely across long uptimes. A silent overwrite leaves
        // the prior `Arc<ServerStats>` still ticked by the original
        // Server instance but invisible to admin SHOW SERVERS, and the
        // new ServerStats starts at zero - masking the old backend's
        // recent activity. Warn and skip; the operator can spot the
        // collision in logs and the actual backend stats live on via
        // the original Server's `stats` field.
        use dashmap::mapref::entry::Entry;
        match SERVER_STATS.entry(server_id) {
            Entry::Occupied(_) => {
                warn!(
                    "server_register: duplicate server_id={server_id} ignored \
                     (likely random-id collision across long uptime)"
                );
            }
            Entry::Vacant(entry) => {
                entry.insert(stats);
            }
        }
    }

    fn server_disconnecting(&self, server_id: i32) {
        SERVER_STATS.remove(&server_id);
    }
}

/// Statistics collector for calculating and updating averages.
///
/// The Collector is responsible for periodically processing the raw statistics
/// data to calculate averages and other derived metrics. It runs as a background
/// task that wakes up at regular intervals (defined by STAT_PERIOD) to perform
/// these calculations.
///
/// There is only one collector instance in the system, which acts as a singleton
/// to ensure consistent statistics processing.
#[derive(Default)]
pub struct Collector {}

impl Collector {
    /// Starts the statistics collection process.
    ///
    /// This method spawns a background task that periodically:
    /// 1. Updates the average statistics for all server connections
    /// 2. Resets the current period counters for the next collection cycle
    /// 3. Prints all statistics for monitoring purposes
    ///
    /// The collection happens every STAT_PERIOD milliseconds (15 seconds by default).
    ///
    /// # Returns
    ///
    /// This method returns immediately after spawning the background task.
    pub async fn collect(&mut self) {
        info!("Stats reporter started");

        tokio::task::spawn(async move {
            // Create a periodic interval for statistics collection
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(STAT_PERIOD));
            // default Burst would fire ALL missed ticks
            // back-to-back after a runtime stall (paging, /metrics scrape
            // contention), thundering the histogram lock paths exactly
            // when they would otherwise drain. Skip means "skip missed
            // ticks, resume the cadence".
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                // Wait for the next interval
                interval.tick().await;

                // snapshot Arc references under shard-local locks, then
                // do all the work off-lock. This used to hold the global
                // RwLock for the full update + reset pass - blocking every
                // client connect/disconnect for ~tens of milliseconds.
                let snapshot: Vec<Arc<ServerStats>> = SERVER_STATS
                    .iter()
                    .map(|entry| entry.value().clone())
                    .collect();

                for stats in &snapshot {
                    if !stats.check_address_stat_average_is_updated_status() {
                        stats.address_stats().update_averages();
                        stats.set_address_stat_average_is_updated_status(true);
                    }
                }

                // Print all collected statistics (reads percentiles from histograms)
                print_all_stats();

                // Reset counters and histograms for the next period.
                reset_period_stats(&snapshot);
            }
        });
    }
}

/// Resets the period counters and histograms once per `AddressStats`.
///
/// Every server in a pool shares one `AddressStats`. `reset_histograms`
/// caches the p50/p90/p95/p99 values into atomics and then empties the
/// histogram, so resetting the same `AddressStats` a second time in one pass
/// reads the now empty histogram and overwrites the cached percentiles with
/// zeros. The `averages_updated` flag is shared per `AddressStats` and set
/// true by the averages pass, so gating on it resets each `AddressStats`
/// exactly once and clears the flag for the next cycle.
fn reset_period_stats(snapshot: &[Arc<ServerStats>]) {
    for stats in snapshot {
        if stats.check_address_stat_average_is_updated_status() {
            stats.address_stats().reset_current_counts();
            stats.address_stats().reset_histograms();
            stats.set_address_stat_average_is_updated_status(false);
        }
    }
}

/// Gets a snapshot of all client statistics.
///
/// This function returns a copy of the current client statistics registry,
/// which can be used for reporting or analysis without affecting the
/// ongoing statistics collection.
///
/// # Returns
///
/// A HashMap mapping client IDs to their corresponding statistics objects
pub fn get_client_stats() -> ClientStatesLookup {
    // shard-local iteration -> HashMap snapshot. Cheaper than the old
    // `clone()` of a held read-lock guard because no full RwLock acquire
    // is involved; admin SHOW CLIENTS path stays consistent.
    CLIENT_STATS
        .iter()
        .map(|entry| (*entry.key(), entry.value().clone()))
        .collect()
}

/// Gets a snapshot of all server statistics.
///
/// This function returns a copy of the current server statistics registry,
/// which can be used for reporting or analysis without affecting the
/// ongoing statistics collection.
///
/// # Returns
///
/// A HashMap mapping server IDs to their corresponding statistics objects
pub fn get_server_stats() -> ServerStatesLookup {
    // shard-local snapshot, see `get_client_stats`.
    SERVER_STATS
        .iter()
        .map(|entry| (*entry.key(), entry.value().clone()))
        .collect()
}

/// Gets the global statistics reporter instance.
///
/// This function provides access to the statistics reporter, which is used
/// to register and unregister clients and servers with the statistics system.
///
/// # Returns
///
/// A clone of the global Reporter instance
pub fn get_reporter() -> Reporter {
    (*(*REPORTER.load())).clone()
}

#[cfg(test)]
mod tests {
    use super::{reset_period_stats, ServerStats};
    use crate::config::Address;
    use crate::stats::address::AddressStats;
    use crate::utils::clock;
    use std::sync::Arc;

    fn server_sharing(address_stats: &Arc<AddressStats>) -> Arc<ServerStats> {
        let address = Address {
            stats: address_stats.clone(),
            ..Address::default()
        };
        Arc::new(ServerStats::new(address, clock::now()))
    }

    #[test]
    fn reset_period_stats_preserves_percentiles_for_multi_server_pool() {
        // Three servers in one pool share a single AddressStats, exactly as
        // the pool builds them.
        let address_stats = Arc::new(AddressStats::default());
        let snapshot = vec![
            server_sharing(&address_stats),
            server_sharing(&address_stats),
            server_sharing(&address_stats),
        ];

        // Populate the histograms so the cached percentiles are non-zero.
        for _ in 0..200 {
            address_stats.xact_time_add(2000);
            address_stats.query_time_add_microseconds(1000);
            address_stats.wait_time_add(500);
        }

        // The averages pass marks the shared AddressStats updated.
        snapshot[0].set_address_stat_average_is_updated_status(true);

        reset_period_stats(&snapshot);

        // reset_histograms ran once, so the cached percentiles hold the
        // recorded latencies instead of being zeroed by the second and third
        // server of the same pool.
        let (_, _, _, p99_query) = address_stats.get_query_percentiles();
        let (_, _, _, p99_xact) = address_stats.get_xact_percentiles();
        let (_, _, _, p99_wait) = address_stats.get_wait_percentiles();
        assert!(
            p99_query > 0,
            "query p99 cache zeroed by multi-server reset: {p99_query}"
        );
        assert!(
            p99_xact > 0,
            "xact p99 cache zeroed by multi-server reset: {p99_xact}"
        );
        assert!(
            p99_wait > 0,
            "wait p99 cache zeroed by multi-server reset: {p99_wait}"
        );

        // The shared flag is cleared so the next cycle's averages pass runs.
        assert!(!snapshot[0].check_address_stat_average_is_updated_status());
    }
}
