use crate::stats::pool::PoolStats;
#[cfg(target_os = "linux")]
use crate::stats::socket::cached_socket_states_count;
#[cfg(target_os = "linux")]
use log::error;
use log::info;

pub fn print_all_stats() {
    let pool_lookup = PoolStats::construct_pool_lookup();
    let mut clients_flag: bool = false;
    pool_lookup.iter().for_each(|(identifier, pool_stats)| {
        let total_clients = pool_stats.cl_waiting
            + pool_stats.cl_idle
            + pool_stats.cl_active
            + pool_stats.cl_cancel_req;
        let total_servers = pool_stats.sv_active + pool_stats.sv_idle;
        if total_clients > 0 {
            clients_flag = true;
            info!(
                "[{}@{}] qps={} tps={} \
                | clients={} active={} idle={} wait={} \
                | servers={} active={} idle={} \
                | query_ms p50={:.2} p90={:.2} p95={:.2} p99={:.2} \
                | xact_ms p50={:.2} p90={:.2} p95={:.2} p99={:.2} \
                | wait_ms p50={:.2} p90={:.2} p95={:.2} p99={:.2} \
                | avg_wait={:.3}ms",
                identifier.user,
                identifier.db,
                pool_stats.avg_query_count,
                pool_stats.avg_xact_count,
                total_clients,
                pool_stats.cl_active,
                pool_stats.cl_idle,
                pool_stats.cl_waiting,
                total_servers,
                pool_stats.sv_active,
                pool_stats.sv_idle,
                pool_stats.query_percentile.p50 as f64 / 1_000f64,
                pool_stats.query_percentile.p90 as f64 / 1_000f64,
                pool_stats.query_percentile.p95 as f64 / 1_000f64,
                pool_stats.query_percentile.p99 as f64 / 1_000f64,
                pool_stats.xact_percentile.p50 as f64 / 1_000f64,
                pool_stats.xact_percentile.p90 as f64 / 1_000f64,
                pool_stats.xact_percentile.p95 as f64 / 1_000f64,
                pool_stats.xact_percentile.p99 as f64 / 1_000f64,
                pool_stats.wait_percentile.p50 as f64 / 1_000f64,
                pool_stats.wait_percentile.p90 as f64 / 1_000f64,
                pool_stats.wait_percentile.p95 as f64 / 1_000f64,
                pool_stats.wait_percentile.p99 as f64 / 1_000f64,
                pool_stats.avg_wait_time as f64 / 1_000f64,
            );
        }
        // surface change/change counters in the periodic log
        // so operators see prewarm/discard/dead-backend activity even
        // when the pool is idle (total_clients == 0). Emitted only when
        // at least one counter is non-zero - keeps the steady-state log
        // line out of the journal during boring uptime.
        let dead_probed = pool_stats.total_dead_backends_probed;
        let dead_evicted = pool_stats.total_dead_backends_evicted;
        let prewarm_failures = pool_stats.total_prewarm_failures;
        let discard_intercepted = pool_stats.total_discard_all_intercepted;
        if dead_probed > 0 || dead_evicted > 0 || prewarm_failures > 0 || discard_intercepted > 0 {
            info!(
                "[{}@{}] maintenance discard_all_intercepted={} prewarm_failures={} \
                dead_backends_probed={} dead_backends_evicted={}",
                identifier.user,
                identifier.db,
                discard_intercepted,
                prewarm_failures,
                dead_probed,
                dead_evicted,
            );
        }
    });
    #[cfg(target_os = "linux")]
    {
        if clients_flag {
            // Background refresher keeps the cache fresh; the periodic
            // stats logger does not need a real-time walk. The EMFILE
            // downgrade from this PR's earlier draft is no longer needed
            // here because no `/proc` walk happens on this code path —
            // it can only return an error if the bootstrap walk in the
            // background refresher itself failed, and that path already
            // logs its own warn.
            match cached_socket_states_count(false) {
                Ok(info) => info!("{}", *info),
                Err(err) => error!("[sockets] error: {err}"),
            };
        }
    }
}
