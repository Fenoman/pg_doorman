use dashmap::DashMap;
use hdrhistogram::Histogram;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::*;

/// Cache-line-aligned wrapper around an `AtomicU64`.
///
/// an `AddressStatFields` struct packs 8 `AtomicU64` fields into a
/// single 64-byte cache line. Two cores incrementing different counters
/// - e.g. one thread bumping `xact_count` while another bumps
/// `query_count` - repeatedly invalidate each other's cache line, costing
/// 30-100 ns per increment under MESI/MOESI coherence traffic on ARM64
/// and EPYC. Padding each hot atomic to its own cache line removes the
/// false-sharing penalty entirely. The cost is 7 × 8 = 56 bytes of
/// padding per atomic (negligible vs the hundreds of MB the pool reserves
/// per backend connection).
#[repr(align(64))]
#[derive(Debug, Default)]
pub struct PaddedAtomicU64(pub AtomicU64);

impl std::ops::Deref for PaddedAtomicU64 {
    type Target = AtomicU64;
    #[inline(always)]
    fn deref(&self) -> &AtomicU64 {
        &self.0
    }
}

/// Fields for tracking various statistics related to PostgreSQL connections by address.
///
/// Each field is an atomic counter allowing safe sharing and updating
/// across multiple threads without additional reference counting.
///
/// the four highest-frequency counters (`xact_count`, `query_count`,
/// `bytes_received`, `bytes_sent`) are wrapped in [`PaddedAtomicU64`] so
/// concurrent writes from different cores do not invalidate each other's
/// cache line. The remaining four (latency totals, wait time, errors) are
/// touched once per query in the slow accounting tail and don't justify
/// the extra padding.
#[derive(Debug, Default)]
pub struct AddressStatFields {
    /// Number of transactions processed
    pub xact_count: PaddedAtomicU64,

    /// Number of queries processed
    pub query_count: PaddedAtomicU64,

    /// Total bytes received from clients
    pub bytes_received: PaddedAtomicU64,

    /// Total bytes sent to clients
    pub bytes_sent: PaddedAtomicU64,

    /// Total transaction processing time in microseconds
    ///
    /// also padded -  originally claimed this was a "slow
    /// accounting tail" but `xact_time_add()` is called on EVERY query,
    /// so it false-shared with `query_time_microseconds` / `wait_time` /
    /// `errors` (all four packed in one 64-byte line). Pad all four to
    /// finish what  started.
    pub xact_time_microseconds: PaddedAtomicU64,

    /// Total query processing time in microseconds
    pub query_time_microseconds: PaddedAtomicU64,

    /// Total time spent waiting for resources in microseconds
    pub wait_time: PaddedAtomicU64,

    /// Number of errors encountered
    pub errors: PaddedAtomicU64,
}

/// Maximum trackable time in microseconds for HDR histogram (10 minutes)
const HISTOGRAM_MAX_VALUE_US: u64 = 10 * 60 * 1_000_000;

/// Canonical PostgreSQL SQLSTATE: exactly 5 uppercase ASCII letters or digits.
/// Bounding the breakdown map's key shape stops adversarial backends from
/// inflating it with arbitrary `ErrorResponse.code` payloads.
fn is_valid_sqlstate(s: &str) -> bool {
    s.len() == 5
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

const MAX_TRACKED_SQLSTATE_CODES: usize = 256;
const SQLSTATE_OTHER_BUCKET: &str = "other";

/// Number of significant digits for HDR histogram precision (3 = 0.1% error)
const HISTOGRAM_SIGFIG: u8 = 2;

/// Creates a new HDR histogram for tracking latencies
fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new_with_max(HISTOGRAM_MAX_VALUE_US, HISTOGRAM_SIGFIG)
        .expect("Failed to create histogram")
}

/// Statistics for PostgreSQL connections grouped by address.
///
/// This struct maintains three sets of statistics:
/// - `total`: Cumulative statistics since the start of the server
/// - `current`: Statistics for the current reporting period
/// - `averages`: Average values calculated from the current period
///
/// It uses HDR histograms for efficient percentile calculations with minimal memory.
#[derive(Debug)]
pub struct AddressStats {
    /// Cumulative statistics since the start of the server
    pub total: AddressStatFields,

    /// Statistics for the current reporting period (reset periodically)
    pub current: AddressStatFields,

    /// Average values calculated from the current period
    pub averages: AddressStatFields,

    /// Flag indicating if the averages have been updated since the last reporting
    pub averages_updated: AtomicBool,

    /// HDR histogram for transaction times in microseconds (reset each period)
    pub xact_histogram: Mutex<Histogram<u64>>,

    /// HDR histogram for query times in microseconds (reset each period)
    pub query_histogram: Mutex<Histogram<u64>>,

    /// HDR histogram for client checkout times in microseconds (reset each period).
    /// Checkout time is the full wait from `ClientStats::waiting()` to the moment
    /// the client receives a server connection — it spans Phase 1/2 semaphore wait,
    /// Phase 4 anticipation, the coordinator path, and the burst gate. Running
    /// mean (`avg_wait_time`) drowns spikes; this histogram is how operators
    /// correlate client-side tail latency with pg_doorman state.
    pub wait_histogram: Mutex<Histogram<u64>>,

    /// Cached p95 transaction time in microseconds. Updated every stats cycle
    /// (15s) from `xact_histogram`. Used by the coordinator's eviction scoring
    /// to prefer slow pools as connection donors — one atomic load per candidate
    /// instead of locking the histogram mutex on every eviction call.
    pub p95_xact_time_us: AtomicU64,

    /// Cached p99 transaction time in microseconds. Updated every stats cycle
    /// (15s) from `xact_histogram`, alongside `p95_xact_time_us`.
    ///
    /// the burst-gate hot path needs `p99_xact_time` to size its
    /// adaptive budget. Previously this called `get_xact_percentiles()`,
    /// which takes a blocking `Mutex` lock on the HDR histogram on every
    /// checkout - a periodic 100+ms stall during the 15s reset cycle, and
    /// constant cacheline contention with the recorder side's `try_lock`.
    /// The atomic cache eliminates both: one `Relaxed` load on the hot
    /// path; the histogram is touched only by the recorder (`try_lock`,
    /// no contention with the reader anymore) and by the 15s collector.
    pub p99_xact_time_us: AtomicU64,

    /// cached query-time percentiles (p50/p90/p95/p99) in
    /// microseconds. Updated every stats cycle (15s) from
    /// `query_histogram` alongside the xact cache. Read by
    /// `initialize_pool_stats` for SHOW POOLS / /api/pools so the hot
    /// scrape path doesn't take a blocking lock on the query
    /// histogram on every Prometheus scrape.
    pub p50_query_time_us: AtomicU64,
    pub p90_query_time_us: AtomicU64,
    pub p95_query_time_us: AtomicU64,
    pub p99_query_time_us: AtomicU64,

    /// cached wait-time percentiles, same shape.
    pub p50_wait_time_us: AtomicU64,
    pub p90_wait_time_us: AtomicU64,
    pub p95_wait_time_us: AtomicU64,
    pub p99_wait_time_us: AtomicU64,

    /// cached xact-time p50/p90 too, for completeness.
    pub p50_xact_time_us: AtomicU64,
    pub p90_xact_time_us: AtomicU64,

    /// Cumulative error counter keyed by PostgreSQL SQLSTATE code (5-char).
    /// Updated alongside `total.errors`. Sharded; the hot path takes a single
    /// shard's read lock for the atomic increment, the slow path inserts a
    /// new shard entry under a brief write lock.
    pub errors_by_sqlstate: DashMap<String, AtomicU64>,

    /// distinct-key count mirror for `errors_by_sqlstate`.
    /// `DashMap::len()` sums every shard under a read lock; on the
    /// unknown-SQLSTATE branch of `error_with_sqlstate` (hot under an
    /// error storm once the cap is reached) that was paid per error. This
    /// atomic is bumped only when `entry` inserts a brand-new key
    /// (including the OTHER bucket), so a relaxed load replaces the
    /// all-shard scan while staying exactly in step with `len()`.
    errors_by_sqlstate_distinct: AtomicUsize,

    /// Total number of idle backends probed by the dead-backend liveness
    /// scan since process start. Bumped once per `check_alive` invocation
    /// inside `Pool::evict_dead_backends`, regardless of outcome - i.e. this
    /// counts ALL backends the scan touched, including healthy ones. The
    /// "actually dead" signal lives in `dead_backends_evicted_total`.
    /// Operators wiring Prometheus alerts on the dead-backend feature should
    /// alert on `dead_backends_evicted_total > 0` (not on probed, which
    /// advances continuously even on a healthy fleet).
    pub dead_backends_probed_total: AtomicU64,

    /// Cumulative number of backends the liveness scan actually dropped
    /// because `check_alive` failed (or the connection was already
    /// `mark_bad`). Each increment corresponds to one fewer entry in
    /// `slots.size`; a sudden growth here followed by `replenish recovered`
    /// log entries is the canonical signature of a PostgreSQL restart that
    /// the scan healed without operator action.
    pub dead_backends_evicted_total: AtomicU64,

    /// Cumulative number of times the configured `prewarm_query` failed on
    /// a newly created backend, causing that backend to be rejected from
    /// the pool. A non-zero value is a config smell: the SQL is wrong, or
    /// it references objects/extensions absent on the target database.
    pub prewarm_failures_total: AtomicU64,

    /// Cumulative number of times the per-pool `intercept_discard_all`
    /// fast path absorbed a client `DISCARD ALL` simple-query (with the
    /// synthetic CommandComplete + ReadyForQuery response). Operators
    /// use this to verify the iServ contract is actually firing - a
    /// regression that silently bypassed the gate (transaction-mode
    /// detection breakage, intercept_discard_all caching drift after
    /// RELOAD, etc.) would leave this counter flat while clients keep
    /// losing temp-table state. Pair with a Prometheus rate-of-change
    /// check for early warning.
    pub discard_all_intercepted_total: AtomicU64,

    /// cumulative number of CancelRequest messages routed at this
    /// pool's address since process start. Populates the `cl_cancel_req`
    /// column in SHOW POOLS (earlier always 0 - dead in production)
    /// so dashboards alerting on cancel storms actually fire.
    pub cancel_requests_total: AtomicU64,

    /// number of HDR histogram samples that were discarded because
    /// `xact_time_add` / `query_time_add_microseconds` / `wait_time_add`
    /// could not acquire `try_lock()`. Under heavy concurrent recording
    /// (the moment when p99 matters most) samples were silently dropped,
    /// biasing percentile reads downward. Operators reading p99 during
    /// an incident need to know if the value is statistically meaningful
    /// - a non-zero rate here means histogram percentiles are
    /// under-counting the slow tail.
    pub histogram_samples_dropped_total: AtomicU64,

    /// Process-unique identifier for this `AddressStats` instance.
    /// Every `Default::default()` mints a fresh value from a static
    /// monotonic counter. The Prometheus scrape path passes this into
    /// the per-pool counter delta tracker so a `Pool::from_config`
    /// recreate (which mints a fresh `AddressStats`) is detected as a
    /// source reset even when the new generation has already grown
    /// past the previous cumulative between two scrapes.
    pub generation: u64,
}

/// Source identifier counter for `AddressStats`. Each fresh instance
/// minted by `Default` reads-and-increments this monotonically so the
/// scrape-side delta tracker can spot pool recreations even when label
/// values are unchanged.
static ADDRESS_STATS_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Returns a unique generation token for the next `AddressStats`.
/// The first observation in any `CounterDeltaTracker` is treated as a
/// reset only when the stored generation differs from this value, so
/// 0 is reserved as the "never observed" sentinel.
pub fn next_address_stats_generation() -> u64 {
    ADDRESS_STATS_GENERATION.fetch_add(1, Ordering::Relaxed)
}

impl Default for AddressStats {
    fn default() -> Self {
        Self {
            total: AddressStatFields::default(),
            current: AddressStatFields::default(),
            averages: AddressStatFields::default(),
            averages_updated: AtomicBool::new(false),
            xact_histogram: Mutex::new(new_histogram()),
            query_histogram: Mutex::new(new_histogram()),
            wait_histogram: Mutex::new(new_histogram()),
            p95_xact_time_us: AtomicU64::new(0),
            p99_xact_time_us: AtomicU64::new(0),
            p50_query_time_us: AtomicU64::new(0),
            p90_query_time_us: AtomicU64::new(0),
            p95_query_time_us: AtomicU64::new(0),
            p99_query_time_us: AtomicU64::new(0),
            p50_wait_time_us: AtomicU64::new(0),
            p90_wait_time_us: AtomicU64::new(0),
            p95_wait_time_us: AtomicU64::new(0),
            p99_wait_time_us: AtomicU64::new(0),
            p50_xact_time_us: AtomicU64::new(0),
            p90_xact_time_us: AtomicU64::new(0),
            errors_by_sqlstate: DashMap::new(),
            errors_by_sqlstate_distinct: AtomicUsize::new(0),
            dead_backends_probed_total: AtomicU64::new(0),
            dead_backends_evicted_total: AtomicU64::new(0),
            prewarm_failures_total: AtomicU64::new(0),
            discard_all_intercepted_total: AtomicU64::new(0),
            histogram_samples_dropped_total: AtomicU64::new(0),
            cancel_requests_total: AtomicU64::new(0),
            generation: next_address_stats_generation(),
        }
    }
}

impl AddressStats {
    /// Bump the dead-backend liveness-scan counters by the work performed in
    /// one `Pool::evict_dead_backends` cycle. `checked` is the number of
    /// backends probed by `check_alive`; `evicted` is how many failed and
    /// were dropped. Callers may pass zero values; the increment is a
    /// no-op then.
    #[inline]
    pub fn record_dead_backend_scan(&self, checked: usize, evicted: usize) {
        if checked > 0 {
            self.dead_backends_probed_total
                .fetch_add(checked as u64, Ordering::Relaxed);
        }
        if evicted > 0 {
            self.dead_backends_evicted_total
                .fetch_add(evicted as u64, Ordering::Relaxed);
        }
    }

    /// Bump `discard_all_intercepted_total`. Called from the synthetic
    /// DISCARD ALL fast path in `Client::handle_simple_query` every
    /// time the iServ gate fires and pg_doorman absorbs a client
    /// `DISCARD ALL` without forwarding to PostgreSQL.
    #[inline]
    pub fn discard_all_intercepted(&self) {
        self.discard_all_intercepted_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// bump the per-pool CancelRequest counter. Called from the
    /// cancel-mode handler when a CancelRequest is routed to a backend
    /// in this address's pool. Populates the `cl_cancel_req` column in
    /// SHOW POOLS (earlier a dead always-zero column).
    #[inline]
    pub fn cancel_request(&self) {
        self.cancel_requests_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bump `prewarm_failures_total`. Called from `ServerPool::run_prewarm_query`
    /// on any SQL/transport failure or cancellation of the configured prewarm
    /// statement; explicit query failures mark the backend bad before the create
    /// path drops it.
    #[inline]
    pub fn prewarm_failure(&self) {
        self.prewarm_failures_total.fetch_add(1, Ordering::Relaxed);
    }
}

/// Converts address statistics into name-value pairs for reporting.
impl IntoIterator for &AddressStats {
    type Item = (String, f64);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    /// Converts the AddressStats into an iterator of (name, value) pairs.
    ///
    /// Total transaction and query times are converted from microseconds to milliseconds
    /// for better readability.
    fn into_iter(self) -> Self::IntoIter {
        vec![
            // Total statistics
            (
                "total_xact_count".to_string(),
                self.total.xact_count.load(Ordering::Relaxed) as f64,
            ),
            (
                "total_query_count".to_string(),
                self.total.query_count.load(Ordering::Relaxed) as f64,
            ),
            (
                "total_received".to_string(),
                self.total.bytes_received.load(Ordering::Relaxed) as f64,
            ),
            (
                "total_sent".to_string(),
                self.total.bytes_sent.load(Ordering::Relaxed) as f64,
            ),
            (
                "total_xact_time".to_string(),
                // Convert microseconds to milliseconds for better readability
                self.total.xact_time_microseconds.load(Ordering::Relaxed) as f64 / 1_000f64,
            ),
            (
                "total_query_time".to_string(),
                // Convert microseconds to milliseconds for better readability
                self.total.query_time_microseconds.load(Ordering::Relaxed) as f64 / 1_000f64,
            ),
            (
                "total_wait_time".to_string(),
                self.total.wait_time.load(Ordering::Relaxed) as f64,
            ),
            (
                "total_errors".to_string(),
                self.total.errors.load(Ordering::Relaxed) as f64,
            ),
            // Average statistics
            (
                "avg_xact_count".to_string(),
                self.averages.xact_count.load(Ordering::Relaxed) as f64,
            ),
            (
                "avg_query_count".to_string(),
                self.averages.query_count.load(Ordering::Relaxed) as f64,
            ),
            (
                "avg_recv".to_string(),
                self.averages.bytes_received.load(Ordering::Relaxed) as f64,
            ),
            (
                "avg_sent".to_string(),
                self.averages.bytes_sent.load(Ordering::Relaxed) as f64,
            ),
            (
                "avg_errors".to_string(),
                self.averages.errors.load(Ordering::Relaxed) as f64,
            ),
            (
                "avg_xact_time".to_string(),
                self.averages.xact_time_microseconds.load(Ordering::Relaxed) as f64,
            ),
            (
                "avg_query_time".to_string(),
                self.averages
                    .query_time_microseconds
                    .load(Ordering::Relaxed) as f64,
            ),
            (
                "avg_wait_time".to_string(),
                self.averages.wait_time.load(Ordering::Relaxed) as f64,
            ),
        ]
        .into_iter()
    }
}

impl AddressStats {
    /// Increments the transaction count in both total and current statistics.
    ///
    /// This method is called whenever a new transaction is started.
    #[inline(always)]
    pub fn xact_count_add(&self) {
        self.total.xact_count.fetch_add(1, Ordering::Relaxed);
        self.current.xact_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Increments the query count in both total and current statistics.
    ///
    /// This method is called whenever a new query is executed.
    #[inline(always)]
    pub fn query_count_add(&self) {
        self.total.query_count.fetch_add(1, Ordering::Relaxed);
        self.current.query_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds the specified number of bytes to the received bytes counter.
    ///
    /// This method is called whenever data is received from a client.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The number of bytes received
    #[inline(always)]
    pub fn bytes_received_add(&self, bytes: u64) {
        self.total
            .bytes_received
            .fetch_add(bytes, Ordering::Relaxed);
        self.current
            .bytes_received
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Adds the specified number of bytes to the sent bytes counter.
    ///
    /// This method is called whenever data is sent to a client.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The number of bytes sent
    #[inline(always)]
    pub fn bytes_sent_add(&self, bytes: u64) {
        self.total.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        self.current.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Adds the specified time to the transaction time counter and records it in the histogram.
    ///
    /// This method records transaction times in an HDR histogram for efficient percentile
    /// calculations. Values exceeding the histogram maximum are clamped.
    ///
    /// # Arguments
    ///
    /// * `microseconds` - The transaction time in microseconds
    #[inline(always)]
    pub fn xact_time_add(&self, microseconds: u64) {
        // Skip recording zero transaction times
        if microseconds == 0 {
            return;
        }

        // Update total and current transaction time counters
        self.total
            .xact_time_microseconds
            .fetch_add(microseconds, Ordering::Relaxed);
        self.current
            .xact_time_microseconds
            .fetch_add(microseconds, Ordering::Relaxed);

        // Record the transaction time in the histogram if we can acquire the lock
        if let Some(mut histogram) = self.xact_histogram.try_lock() {
            // Clamp value to histogram max to avoid errors
            let value = microseconds.min(HISTOGRAM_MAX_VALUE_US);
            let _ = histogram.record(value);
        } else {
            // bump the drop counter so operators know percentiles
            // are biased low under contention. The hot path stays
            // non-blocking.
            self.histogram_samples_dropped_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Adds the specified time to the query time counter and records it in the histogram.
    ///
    /// This method records query times in an HDR histogram for efficient percentile
    /// calculations. Values exceeding the histogram maximum are clamped.
    ///
    /// # Arguments
    ///
    /// * `microseconds` - The query time in microseconds
    #[inline(always)]
    pub fn query_time_add_microseconds(&self, microseconds: u64) {
        // Update total and current query time counters
        self.total
            .query_time_microseconds
            .fetch_add(microseconds, Ordering::Relaxed);
        self.current
            .query_time_microseconds
            .fetch_add(microseconds, Ordering::Relaxed);

        // Record the query time in the histogram if we can acquire the lock
        if let Some(mut histogram) = self.query_histogram.try_lock() {
            // Clamp value to histogram max to avoid errors
            let value = microseconds.min(HISTOGRAM_MAX_VALUE_US);
            let _ = histogram.record(value);
        } else {
            // see xact_time_add - operators need visibility into
            // contention-driven sample loss.
            self.histogram_samples_dropped_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Adds the specified time to the wait time counter and records it in
    /// the wait histogram.
    ///
    /// Called from `ServerStats::checkout_time` on every successful client
    /// checkout. The histogram captures per-checkout tail latency that the
    /// `avg_wait_time` running mean washes out.
    ///
    /// # Arguments
    ///
    /// * `time` - The wait time in microseconds
    #[inline(always)]
    pub fn wait_time_add(&self, time: u64) {
        self.total.wait_time.fetch_add(time, Ordering::Relaxed);
        self.current.wait_time.fetch_add(time, Ordering::Relaxed);

        // Record the wait time in the histogram if we can acquire the lock.
        // Matches the `try_lock` discipline of query/xact paths: the hot
        // path never blocks on the stats mutex. Zero-length checkouts are
        // still recorded — they are the healthy-pool baseline.
        if let Some(mut histogram) = self.wait_histogram.try_lock() {
            let value = time.min(HISTOGRAM_MAX_VALUE_US);
            let _ = histogram.record(value);
        } else {
            // see xact_time_add.
            self.histogram_samples_dropped_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Increments the error counter in both total and current statistics.
    ///
    /// This method is called whenever an error occurs during query processing.
    #[inline(always)]
    pub fn error(&self) {
        self.total.errors.fetch_add(1, Ordering::Relaxed);
        self.current.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the total/current error counters and the per-SQLSTATE
    /// breakdown bucket. After the first observation of a given code the hot
    /// path takes one DashMap shard read lock; the first observation also
    /// takes a brief shard write lock to allocate the bucket.
    ///
    /// Codes that fail validation (`is_valid_sqlstate`) are counted in the
    /// aggregate `errors` counter but skipped from the breakdown so a
    /// malformed or adversarial `ErrorResponse.code` field cannot grow the
    /// map without bound. Canonical PostgreSQL SQLSTATEs are exactly 5
    /// uppercase-ASCII characters.
    #[inline(always)]
    pub fn error_with_sqlstate(&self, sqlstate: &str) {
        self.error();
        if !is_valid_sqlstate(sqlstate) {
            return;
        }
        // borrow-only fast path for an already-tracked code -
        // a shard read lock and an atomic increment, no allocation. The
        // per-error `to_string()` now runs only when a new key must be
        // inserted (the cold path). Apps repeat a small set of SQLSTATEs,
        // so this is the common case.
        if let Some(counter) = self.errors_by_sqlstate.get(sqlstate) {
            counter.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // read the distinct-key mirror instead of
        // `DashMap::len()` (which locks every shard) so an unknown
        // SQLSTATE under an error storm - once the cap is reached, every
        // fresh code folds into OTHER - does not pay an all-shard scan
        // per error.
        let bucket = if self.errors_by_sqlstate_distinct.load(Ordering::Relaxed)
            < MAX_TRACKED_SQLSTATE_CODES
        {
            sqlstate
        } else {
            SQLSTATE_OTHER_BUCKET
        };
        // Bump the distinct-key mirror exactly when `entry` inserts a
        // brand-new key (including the OTHER bucket), keeping it in step
        // with `errors_by_sqlstate.len()`. The OTHER bucket may already
        // exist here (a prior overflow created it), so still match.
        match self.errors_by_sqlstate.entry(bucket.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(occupied) => {
                occupied.get().fetch_add(1, Ordering::Relaxed);
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                self.errors_by_sqlstate_distinct
                    .fetch_add(1, Ordering::Relaxed);
                vacant.insert(AtomicU64::new(1));
            }
        }
    }

    /// Snapshot of the SQLSTATE breakdown as a plain map. Reading is O(N)
    /// over the live entries with one atomic load each.
    pub fn errors_by_sqlstate_snapshot(&self) -> HashMap<String, u64> {
        self.errors_by_sqlstate
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().load(Ordering::Relaxed)))
            .collect()
    }

    /// Returns transaction time percentiles (p50, p90, p95, p99) in microseconds.
    ///
    /// read the cached atomic snapshot maintained by
    /// `reset_histograms` (refreshed every 15s by the Collector).
    /// Prior shape took a blocking `xact_histogram.lock()` here,
    /// contending against `reset_histograms()` and against recorders
    /// that use `try_lock` and increment
    /// `histogram_samples_dropped_total`. Under a `/metrics` scrape
    /// storm during the 15s collector tick, reader threads serialised
    /// on each pool's lock.
    pub fn get_xact_percentiles(&self) -> (u64, u64, u64, u64) {
        (
            self.p50_xact_time_us.load(Ordering::Relaxed),
            self.p90_xact_time_us.load(Ordering::Relaxed),
            self.p95_xact_time_us.load(Ordering::Relaxed),
            self.p99_xact_time_us.load(Ordering::Relaxed),
        )
    }

    /// Returns query time percentiles (p50, p90, p95, p99) in microseconds.
    /// See `get_xact_percentiles` for the cache rationale.
    pub fn get_query_percentiles(&self) -> (u64, u64, u64, u64) {
        (
            self.p50_query_time_us.load(Ordering::Relaxed),
            self.p90_query_time_us.load(Ordering::Relaxed),
            self.p95_query_time_us.load(Ordering::Relaxed),
            self.p99_query_time_us.load(Ordering::Relaxed),
        )
    }

    /// Returns client checkout (wait) time percentiles (p50, p90, p95, p99)
    /// in microseconds.
    ///
    /// Checkout time covers the full journey of `Pool::timeout_get`:
    /// semaphore wait, Phase 4 anticipation, coordinator Phase A/R/B/C/D,
    /// pre-create recycle, burst gate, and `server_pool.create()`. Use
    /// the p99 value to correlate client-side latency spikes against the
    /// pool. See `get_xact_percentiles` for the cache rationale.
    pub fn get_wait_percentiles(&self) -> (u64, u64, u64, u64) {
        (
            self.p50_wait_time_us.load(Ordering::Relaxed),
            self.p90_wait_time_us.load(Ordering::Relaxed),
            self.p95_wait_time_us.load(Ordering::Relaxed),
            self.p99_wait_time_us.load(Ordering::Relaxed),
        )
    }

    /// Resets the histograms for the new time window.
    ///
    /// Called at the end of each stats period (15 seconds) to start fresh.
    pub fn refresh_percentile_cache(&self) {
        {
            let histogram = self.xact_histogram.lock();
            self.p50_xact_time_us
                .store(histogram.value_at_quantile(0.50), Ordering::Relaxed);
            self.p90_xact_time_us
                .store(histogram.value_at_quantile(0.90), Ordering::Relaxed);
            self.p95_xact_time_us
                .store(histogram.value_at_quantile(0.95), Ordering::Relaxed);
            self.p99_xact_time_us
                .store(histogram.value_at_quantile(0.99), Ordering::Relaxed);
        }
        {
            let histogram = self.query_histogram.lock();
            self.p50_query_time_us
                .store(histogram.value_at_quantile(0.50), Ordering::Relaxed);
            self.p90_query_time_us
                .store(histogram.value_at_quantile(0.90), Ordering::Relaxed);
            self.p95_query_time_us
                .store(histogram.value_at_quantile(0.95), Ordering::Relaxed);
            self.p99_query_time_us
                .store(histogram.value_at_quantile(0.99), Ordering::Relaxed);
        }
        {
            let histogram = self.wait_histogram.lock();
            self.p50_wait_time_us
                .store(histogram.value_at_quantile(0.50), Ordering::Relaxed);
            self.p90_wait_time_us
                .store(histogram.value_at_quantile(0.90), Ordering::Relaxed);
            self.p95_wait_time_us
                .store(histogram.value_at_quantile(0.95), Ordering::Relaxed);
            self.p99_wait_time_us
                .store(histogram.value_at_quantile(0.99), Ordering::Relaxed);
        }
    }

    pub fn reset_histograms(&self) {
        // Cache p50/p90/p95/p99 for query_histogram and wait_histogram
        // alongside xact_histogram, so SHOW POOLS and initialize_pool_stats
        // can read percentiles from atomic loads instead of taking histogram
        // locks per pool per scrape.
        {
            let mut histogram = self.xact_histogram.lock();
            self.p50_xact_time_us
                .store(histogram.value_at_quantile(0.50), Ordering::Relaxed);
            self.p90_xact_time_us
                .store(histogram.value_at_quantile(0.90), Ordering::Relaxed);
            self.p95_xact_time_us
                .store(histogram.value_at_quantile(0.95), Ordering::Relaxed);
            self.p99_xact_time_us
                .store(histogram.value_at_quantile(0.99), Ordering::Relaxed);
            histogram.reset();
        }
        {
            let mut histogram = self.query_histogram.lock();
            self.p50_query_time_us
                .store(histogram.value_at_quantile(0.50), Ordering::Relaxed);
            self.p90_query_time_us
                .store(histogram.value_at_quantile(0.90), Ordering::Relaxed);
            self.p95_query_time_us
                .store(histogram.value_at_quantile(0.95), Ordering::Relaxed);
            self.p99_query_time_us
                .store(histogram.value_at_quantile(0.99), Ordering::Relaxed);
            histogram.reset();
        }
        {
            let mut histogram = self.wait_histogram.lock();
            self.p50_wait_time_us
                .store(histogram.value_at_quantile(0.50), Ordering::Relaxed);
            self.p90_wait_time_us
                .store(histogram.value_at_quantile(0.90), Ordering::Relaxed);
            self.p95_wait_time_us
                .store(histogram.value_at_quantile(0.95), Ordering::Relaxed);
            self.p99_wait_time_us
                .store(histogram.value_at_quantile(0.99), Ordering::Relaxed);
            histogram.reset();
        }
    }

    /// Updates the average statistics based on the current period's values.
    ///
    /// This method calculates per-second averages for all metrics and average times per transaction/query.
    /// It is called periodically by the stats collector to update the reported averages.
    pub fn update_averages(&self) {
        // Convert the stat period from milliseconds to seconds for per-second calculations
        // floor at 1 so a future change that drops
        // STAT_PERIOD below 1000 ms (e.g. for testing) cannot
        // cause integer-division-by-zero panic on every tick.
        let stat_period_per_second = (crate::stats::STAT_PERIOD / 1_000).max(1);

        // Calculate transaction-related averages
        self.update_transaction_averages(stat_period_per_second);

        // Calculate query-related averages
        self.update_query_averages(stat_period_per_second);

        // Calculate throughput averages (bytes received/sent)
        self.update_throughput_averages(stat_period_per_second);

        // Calculate wait time and error averages
        self.update_wait_and_error_averages(stat_period_per_second);
    }

    fn update_transaction_averages(&self, stat_period_per_second: u64) {
        let current_xact_count = self.current.xact_count.load(Ordering::Relaxed);
        let current_xact_time = self.current.xact_time_microseconds.load(Ordering::Relaxed);

        self.averages.xact_count.store(
            current_xact_count / stat_period_per_second,
            Ordering::Relaxed,
        );

        if current_xact_count == 0 {
            self.averages
                .xact_time_microseconds
                .store(0, Ordering::Relaxed);
        } else {
            self.averages
                .xact_time_microseconds
                .store(current_xact_time / current_xact_count, Ordering::Relaxed);
        }
    }

    fn update_query_averages(&self, stat_period_per_second: u64) {
        let current_query_count = self.current.query_count.load(Ordering::Relaxed);
        let current_query_time = self.current.query_time_microseconds.load(Ordering::Relaxed);

        self.averages.query_count.store(
            current_query_count / stat_period_per_second,
            Ordering::Relaxed,
        );

        if current_query_count == 0 {
            self.averages
                .query_time_microseconds
                .store(0, Ordering::Relaxed);
        } else {
            self.averages
                .query_time_microseconds
                .store(current_query_time / current_query_count, Ordering::Relaxed);
        }
    }

    fn update_throughput_averages(&self, stat_period_per_second: u64) {
        let current_bytes_received = self.current.bytes_received.load(Ordering::Relaxed);
        self.averages.bytes_received.store(
            current_bytes_received / stat_period_per_second,
            Ordering::Relaxed,
        );

        let current_bytes_sent = self.current.bytes_sent.load(Ordering::Relaxed);
        self.averages.bytes_sent.store(
            current_bytes_sent / stat_period_per_second,
            Ordering::Relaxed,
        );
    }

    fn update_wait_and_error_averages(&self, stat_period_per_second: u64) {
        let current_wait_time = self.current.wait_time.load(Ordering::Relaxed);
        self.averages.wait_time.store(
            current_wait_time / stat_period_per_second,
            Ordering::Relaxed,
        );

        let current_errors = self.current.errors.load(Ordering::Relaxed);
        self.averages
            .errors
            .store(current_errors / stat_period_per_second, Ordering::Relaxed);
    }

    /// Resets all current period counters to zero.
    ///
    /// This method is called after the averages have been updated to prepare for the next period.
    ///
    /// Use `swap(0)` instead of `store(0)` so increments racing with
    /// the reset are either returned by the swap for this period or
    /// preserved for the next period. No increment can be wiped between
    /// the average update and the reset.
    pub fn reset_current_counts(&self) {
        // Reset transaction-related counters
        let _ = self.current.xact_count.swap(0, Ordering::Relaxed);
        let _ = self
            .current
            .xact_time_microseconds
            .swap(0, Ordering::Relaxed);

        // Reset query-related counters
        let _ = self.current.query_count.swap(0, Ordering::Relaxed);
        let _ = self
            .current
            .query_time_microseconds
            .swap(0, Ordering::Relaxed);

        // Reset throughput counters
        let _ = self.current.bytes_received.swap(0, Ordering::Relaxed);
        let _ = self.current.bytes_sent.swap(0, Ordering::Relaxed);

        // Reset wait time and error counters
        let _ = self.current.wait_time.swap(0, Ordering::Relaxed);
        let _ = self.current.errors.swap(0, Ordering::Relaxed);
    }

    /// Populates a row vector with string representations of all statistics.
    ///
    /// This method is used for generating reports or displaying statistics in a tabular format.
    ///
    /// # Arguments
    ///
    /// * `row` - A mutable reference to a vector of strings that will be populated with statistics
    pub fn populate_row(&self, row: &mut Vec<String>) {
        for (_key, value) in self {
            row.push(value.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_address_stat_fields_default() {
        let fields = AddressStatFields::default();

        assert_eq!(fields.xact_count.load(Ordering::Relaxed), 0);
        assert_eq!(fields.query_count.load(Ordering::Relaxed), 0);
        assert_eq!(fields.bytes_received.load(Ordering::Relaxed), 0);
        assert_eq!(fields.bytes_sent.load(Ordering::Relaxed), 0);
        assert_eq!(fields.xact_time_microseconds.load(Ordering::Relaxed), 0);
        assert_eq!(fields.query_time_microseconds.load(Ordering::Relaxed), 0);
        assert_eq!(fields.wait_time.load(Ordering::Relaxed), 0);
        assert_eq!(fields.errors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_address_stats_default() {
        let stats = AddressStats::default();

        assert_eq!(stats.total.xact_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.total.query_count.load(Ordering::Relaxed), 0);

        assert_eq!(stats.current.xact_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.current.query_count.load(Ordering::Relaxed), 0);

        assert_eq!(stats.averages.xact_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.averages.query_count.load(Ordering::Relaxed), 0);

        assert!(!stats.averages_updated.load(Ordering::Relaxed));
        assert_eq!(stats.xact_histogram.lock().len(), 0);
        assert_eq!(stats.query_histogram.lock().len(), 0);
    }

    #[test]
    fn test_counter_methods() {
        let stats = AddressStats::default();

        stats.xact_count_add();
        assert_eq!(stats.total.xact_count.load(Ordering::Relaxed), 1);
        assert_eq!(stats.current.xact_count.load(Ordering::Relaxed), 1);

        stats.query_count_add();
        assert_eq!(stats.total.query_count.load(Ordering::Relaxed), 1);
        assert_eq!(stats.current.query_count.load(Ordering::Relaxed), 1);

        stats.bytes_received_add(100);
        assert_eq!(stats.total.bytes_received.load(Ordering::Relaxed), 100);
        assert_eq!(stats.current.bytes_received.load(Ordering::Relaxed), 100);

        stats.bytes_sent_add(200);
        assert_eq!(stats.total.bytes_sent.load(Ordering::Relaxed), 200);
        assert_eq!(stats.current.bytes_sent.load(Ordering::Relaxed), 200);

        stats.wait_time_add(300);
        assert_eq!(stats.total.wait_time.load(Ordering::Relaxed), 300);
        assert_eq!(stats.current.wait_time.load(Ordering::Relaxed), 300);

        stats.error();
        assert_eq!(stats.total.errors.load(Ordering::Relaxed), 1);
        assert_eq!(stats.current.errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_error_with_sqlstate_breakdown() {
        let stats = AddressStats::default();

        stats.error_with_sqlstate("23503");
        stats.error_with_sqlstate("23503");
        stats.error_with_sqlstate("57P01");
        stats.error_with_sqlstate("53300");

        // The aggregate counter increments alongside the per-code bucket.
        assert_eq!(stats.total.errors.load(Ordering::Relaxed), 4);
        assert_eq!(stats.current.errors.load(Ordering::Relaxed), 4);

        let snap = stats.errors_by_sqlstate_snapshot();
        assert_eq!(snap.get("23503"), Some(&2));
        assert_eq!(snap.get("57P01"), Some(&1));
        assert_eq!(snap.get("53300"), Some(&1));
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn test_error_with_sqlstate_snapshot_empty_by_default() {
        let stats = AddressStats::default();
        // No PG-side errors yet — breakdown is empty even after a plain
        // `error()` call. Only `error_with_sqlstate` populates the bucket.
        stats.error();
        assert_eq!(stats.total.errors.load(Ordering::Relaxed), 1);
        assert!(stats.errors_by_sqlstate_snapshot().is_empty());
    }

    #[test]
    fn test_error_with_sqlstate_rejects_malformed_codes() {
        let stats = AddressStats::default();

        // Aggregate counter advances for every call, but only canonical
        // codes land in the breakdown.
        stats.error_with_sqlstate("23503"); // valid
        stats.error_with_sqlstate("");
        stats.error_with_sqlstate("23503EXTRA");
        stats.error_with_sqlstate("aBc12");
        stats.error_with_sqlstate("23 03");
        stats.error_with_sqlstate("23503\u{0}");

        assert_eq!(stats.total.errors.load(Ordering::Relaxed), 6);
        let snap = stats.errors_by_sqlstate_snapshot();
        assert_eq!(snap.get("23503"), Some(&1));
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn test_error_with_sqlstate_caps_distinct_codes() {
        let stats = AddressStats::default();

        for i in 0..(MAX_TRACKED_SQLSTATE_CODES + 8) {
            stats.error_with_sqlstate(&format!("Z{i:04}"));
        }

        let snap = stats.errors_by_sqlstate_snapshot();
        assert!(
            snap.len() <= MAX_TRACKED_SQLSTATE_CODES + 1,
            "snapshot grew beyond cap: {}",
            snap.len()
        );
        assert_eq!(snap.get(SQLSTATE_OTHER_BUCKET), Some(&8));
    }

    #[test]
    fn test_is_valid_sqlstate() {
        // Canonical codes accepted across the documented PG classes.
        assert!(is_valid_sqlstate("00000"));
        assert!(is_valid_sqlstate("23503"));
        assert!(is_valid_sqlstate("57P01"));
        assert!(is_valid_sqlstate("ZZZZZ"));

        // Anything else rejected.
        assert!(!is_valid_sqlstate(""));
        assert!(!is_valid_sqlstate("2350"));
        assert!(!is_valid_sqlstate("235033"));
        assert!(!is_valid_sqlstate("aBcDe"));
        assert!(!is_valid_sqlstate("01-23"));
    }

    #[test]
    fn test_time_recording_methods() {
        let stats = AddressStats::default();

        stats.xact_time_add(150);
        assert_eq!(
            stats.total.xact_time_microseconds.load(Ordering::Relaxed),
            150
        );
        assert_eq!(
            stats.current.xact_time_microseconds.load(Ordering::Relaxed),
            150
        );

        {
            let histogram = stats.xact_histogram.lock();
            assert_eq!(histogram.len(), 1);
        }

        stats.xact_time_add(0);
        assert_eq!(
            stats.total.xact_time_microseconds.load(Ordering::Relaxed),
            150
        ); // Unchanged
        assert_eq!(
            stats.current.xact_time_microseconds.load(Ordering::Relaxed),
            150
        ); // Unchanged

        stats.query_time_add_microseconds(250);
        assert_eq!(
            stats.total.query_time_microseconds.load(Ordering::Relaxed),
            250
        );
        assert_eq!(
            stats
                .current
                .query_time_microseconds
                .load(Ordering::Relaxed),
            250
        );

        {
            let histogram = stats.query_histogram.lock();
            assert_eq!(histogram.len(), 1);
        }
    }

    #[test]
    fn test_histogram_percentiles() {
        let stats = AddressStats::default();

        for i in 1..=100 {
            stats.xact_time_add(i as u64);
            stats.query_time_add_microseconds(i as u64);
        }

        // `get_*_percentiles` now read the cached
        // atomic snapshot maintained by `reset_histograms` (every
        // 15s by the Collector in production). In tests we must
        // call `reset_histograms` explicitly to refresh the cache
        // from the live histogram data before reading.
        stats.reset_histograms();

        let (p50, p90, p95, p99) = stats.get_xact_percentiles();
        assert!(
            (45..=55).contains(&p50),
            "p50 xact should be ~50, got {p50}"
        );
        assert!(
            (85..=95).contains(&p90),
            "p90 xact should be ~90, got {p90}"
        );
        assert!(
            (90..=100).contains(&p95),
            "p95 xact should be ~95, got {p95}"
        );
        assert!(
            (95..=105).contains(&p99),
            "p99 xact should be ~99, got {p99}"
        );

        let (p50, p90, p95, p99) = stats.get_query_percentiles();
        assert!(
            (45..=55).contains(&p50),
            "p50 query should be ~50, got {p50}"
        );
        assert!(
            (85..=95).contains(&p90),
            "p90 query should be ~90, got {p90}"
        );
        assert!(
            (90..=100).contains(&p95),
            "p95 query should be ~95, got {p95}"
        );
        assert!(
            (95..=105).contains(&p99),
            "p99 query should be ~99, got {p99}"
        );
    }

    #[test]
    fn refresh_percentile_cache_publishes_current_window_without_reset() {
        let stats = AddressStats::default();

        for value in [100, 100, 100, 200] {
            stats.query_time_add_microseconds(value);
        }

        assert_eq!(
            stats.get_query_percentiles(),
            (0, 0, 0, 0),
            "cached percentile atoms should start cold before collector/admin refresh"
        );

        stats.refresh_percentile_cache();
        let (p50, _p90, _p95, p99) = stats.get_query_percentiles();
        assert!(
            (90..=110).contains(&p50),
            "fresh p50 query should be ~100us, got {p50}"
        );
        assert!(
            (190..=210).contains(&p99),
            "fresh p99 query should be ~200us, got {p99}"
        );
        assert_eq!(
            stats.query_histogram.lock().len(),
            4,
            "admin refresh must not reset the collector window"
        );
    }

    #[test]
    fn test_histogram_reset() {
        let stats = AddressStats::default();

        for i in 1..=10 {
            stats.xact_time_add(i as u64);
        }

        assert_eq!(stats.xact_histogram.lock().len(), 10);

        stats.reset_histograms();

        assert_eq!(stats.xact_histogram.lock().len(), 0);
    }

    #[test]
    fn test_update_averages_and_reset() {
        let stats = AddressStats::default();

        stats.xact_count_add();
        stats.xact_count_add();
        stats.xact_time_add(1000); // 1000 microseconds for first transaction
        stats.xact_time_add(2000); // 2000 microseconds for second transaction

        stats.query_count_add();
        stats.query_count_add();
        stats.query_count_add();
        stats.query_time_add_microseconds(300); // 300 microseconds for first query
        stats.query_time_add_microseconds(400); // 400 microseconds for second query
        stats.query_time_add_microseconds(500); // 500 microseconds for third query

        stats.bytes_received_add(15000);
        stats.bytes_sent_add(25000);
        stats.wait_time_add(500);
        stats.error();
        stats.error();

        stats.update_averages();

        assert_eq!(stats.averages.xact_count.load(Ordering::Relaxed), 0);
        assert_eq!(
            stats
                .averages
                .xact_time_microseconds
                .load(Ordering::Relaxed),
            1500
        );

        assert_eq!(stats.averages.query_count.load(Ordering::Relaxed), 0);
        assert_eq!(
            stats
                .averages
                .query_time_microseconds
                .load(Ordering::Relaxed),
            400
        );

        assert_eq!(
            stats.averages.bytes_received.load(Ordering::Relaxed),
            15000 / 15
        );
        assert_eq!(
            stats.averages.bytes_sent.load(Ordering::Relaxed),
            25000 / 15
        );

        assert_eq!(stats.averages.wait_time.load(Ordering::Relaxed), 500 / 15);
        assert_eq!(stats.averages.errors.load(Ordering::Relaxed), 2 / 15);

        stats.reset_current_counts();

        assert_eq!(stats.current.xact_count.load(Ordering::Relaxed), 0);
        assert_eq!(
            stats.current.xact_time_microseconds.load(Ordering::Relaxed),
            0
        );
        assert_eq!(stats.current.query_count.load(Ordering::Relaxed), 0);
        assert_eq!(
            stats
                .current
                .query_time_microseconds
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(stats.current.bytes_received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.current.bytes_sent.load(Ordering::Relaxed), 0);
        assert_eq!(stats.current.wait_time.load(Ordering::Relaxed), 0);
        assert_eq!(stats.current.errors.load(Ordering::Relaxed), 0);

        assert_eq!(stats.total.xact_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            stats.total.xact_time_microseconds.load(Ordering::Relaxed),
            3000
        );
        assert_eq!(stats.total.query_count.load(Ordering::Relaxed), 3);
        assert_eq!(
            stats.total.query_time_microseconds.load(Ordering::Relaxed),
            1200
        );
    }

    #[test]
    fn test_into_iterator() {
        let stats = AddressStats::default();

        stats.total.xact_count.store(10, Ordering::Relaxed);
        stats.total.query_count.store(20, Ordering::Relaxed);
        stats.total.bytes_received.store(1000, Ordering::Relaxed);
        stats.total.bytes_sent.store(2000, Ordering::Relaxed);
        stats
            .total
            .xact_time_microseconds
            .store(5000, Ordering::Relaxed);
        stats
            .total
            .query_time_microseconds
            .store(6000, Ordering::Relaxed);
        stats.total.wait_time.store(300, Ordering::Relaxed);
        stats.total.errors.store(5, Ordering::Relaxed);

        stats.averages.xact_count.store(2, Ordering::Relaxed);
        stats.averages.query_count.store(4, Ordering::Relaxed);
        stats.averages.bytes_received.store(200, Ordering::Relaxed);
        stats.averages.bytes_sent.store(400, Ordering::Relaxed);
        stats
            .averages
            .xact_time_microseconds
            .store(500, Ordering::Relaxed);
        stats
            .averages
            .query_time_microseconds
            .store(300, Ordering::Relaxed);
        stats.averages.wait_time.store(30, Ordering::Relaxed);
        stats.averages.errors.store(1, Ordering::Relaxed);

        let stats_map: HashMap<String, f64> = (&stats).into_iter().collect();

        assert_eq!(stats_map.get("total_xact_count"), Some(&10.0));
        assert_eq!(stats_map.get("total_query_count"), Some(&20.0));
        assert_eq!(stats_map.get("total_received"), Some(&1000.0));
        assert_eq!(stats_map.get("total_sent"), Some(&2000.0));
        assert_eq!(stats_map.get("total_xact_time"), Some(&5.0)); // Converted to milliseconds
        assert_eq!(stats_map.get("total_query_time"), Some(&6.0)); // Converted to milliseconds
        assert_eq!(stats_map.get("total_wait_time"), Some(&300.0));
        assert_eq!(stats_map.get("total_errors"), Some(&5.0));

        assert_eq!(stats_map.get("avg_xact_count"), Some(&2.0));
        assert_eq!(stats_map.get("avg_query_count"), Some(&4.0));
        assert_eq!(stats_map.get("avg_recv"), Some(&200.0));
        assert_eq!(stats_map.get("avg_sent"), Some(&400.0));
        assert_eq!(stats_map.get("avg_xact_time"), Some(&500.0));
        assert_eq!(stats_map.get("avg_query_time"), Some(&300.0));
        assert_eq!(stats_map.get("avg_wait_time"), Some(&30.0));
        assert_eq!(stats_map.get("avg_errors"), Some(&1.0));
    }

    #[test]
    fn test_populate_row() {
        let stats = AddressStats::default();

        stats.total.xact_count.store(10, Ordering::Relaxed);
        stats.total.query_count.store(20, Ordering::Relaxed);

        let mut row = Vec::new();

        stats.populate_row(&mut row);

        assert_eq!(row.len(), 16); // 8 total stats + 8 average stats

        assert_eq!(row[0], "10");

        assert_eq!(row[1], "20");
    }

    #[test]
    fn test_thread_safety() {
        let stats = Arc::new(AddressStats::default());
        let mut handles = vec![];

        for _ in 0..10 {
            let stats_clone = Arc::clone(&stats);
            let handle = thread::spawn(move || {
                for _ in 0..100 {
                    stats_clone.xact_count_add();
                    stats_clone.query_count_add();
                    stats_clone.bytes_received_add(10);
                    stats_clone.bytes_sent_add(20);
                    stats_clone.xact_time_add(5);
                    stats_clone.query_time_add_microseconds(3);
                    stats_clone.wait_time_add(2);
                    stats_clone.error();

                    // Small sleep to increase chance of thread interleaving
                    thread::sleep(Duration::from_micros(1));
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(stats.total.xact_count.load(Ordering::Relaxed), 1000); // 10 threads * 100 increments
        assert_eq!(stats.total.query_count.load(Ordering::Relaxed), 1000);
        assert_eq!(stats.total.bytes_received.load(Ordering::Relaxed), 10000); // 10 threads * 100 * 10 bytes
        assert_eq!(stats.total.bytes_sent.load(Ordering::Relaxed), 20000); // 10 threads * 100 * 20 bytes
        assert_eq!(
            stats.total.xact_time_microseconds.load(Ordering::Relaxed),
            5000
        ); // 10 threads * 100 * 5 microseconds
        assert_eq!(
            stats.total.query_time_microseconds.load(Ordering::Relaxed),
            3000
        ); // 10 threads * 100 * 3 microseconds
        assert_eq!(stats.total.wait_time.load(Ordering::Relaxed), 2000); // 10 threads * 100 * 2 microseconds
        assert_eq!(stats.total.errors.load(Ordering::Relaxed), 1000); // 10 threads * 100 errors

        assert_eq!(stats.current.xact_count.load(Ordering::Relaxed), 1000);
        assert_eq!(stats.current.query_count.load(Ordering::Relaxed), 1000);
        assert_eq!(stats.current.bytes_received.load(Ordering::Relaxed), 10000);
        assert_eq!(stats.current.bytes_sent.load(Ordering::Relaxed), 20000);
        assert_eq!(
            stats.current.xact_time_microseconds.load(Ordering::Relaxed),
            5000
        );
        assert_eq!(
            stats
                .current
                .query_time_microseconds
                .load(Ordering::Relaxed),
            3000
        );
        assert_eq!(stats.current.wait_time.load(Ordering::Relaxed), 2000);
        assert_eq!(stats.current.errors.load(Ordering::Relaxed), 1000);
    }
}
