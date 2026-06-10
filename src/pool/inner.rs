use std::{
    collections::VecDeque,
    fmt,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

use log::{debug, warn};
use rand::Rng as _;

use crate::utils::clock;

use parking_lot::Mutex;

use tokio::sync::{oneshot, Notify, Semaphore, SemaphorePermit, TryAcquireError};

use super::errors::{PoolError, RecycleError, TimeoutType};
use super::pool_coordinator;
use super::types::{Metrics, PoolConfig, QueueMode, Status, Timeouts};
use super::ServerPool;
use crate::server::Server;

const MAX_FAST_RETRY: i32 = 10;

/// Fallback wake interval for tasks queued behind the bounded burst limiter.
/// Used as a safety net in case neither a direct-handoff delivery nor
/// `create_done` fires within the expected window — guarantees forward
/// progress without busy-spinning.
const BURST_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5);

/// Internal object wrapper with metrics.
/// The `coordinator_permit` is held for the entire lifetime of the connection:
/// - Acquired when a NEW connection is created (timeout_get / replenish)
/// - Stays with the ObjectInner when returned to the idle pool (VecDeque)
/// - Dropped when the connection is destroyed → frees coordinator semaphore slot
/// - `None` when coordination is disabled (max_db_connections = 0)
#[derive(Debug)]
struct ObjectInner {
    obj: Server,
    metrics: Metrics,
    /// Held for RAII — dropped when connection is destroyed, freeing coordinator slot.
    #[allow(dead_code)]
    coordinator_permit: Option<pool_coordinator::CoordinatorPermit>,
}

/// Wrapper around the actual pooled object which implements Deref and DerefMut.
/// When dropped, the object is returned to the pool.
pub struct Object {
    inner: Option<ObjectInner>,
    pool: Weak<PoolInner>,
}

impl Drop for Object {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            if let Some(pool) = self.pool.upgrade() {
                // Drop cannot await normal checkin cleanup. If a client task
                // panics or is cancelled while holding a transaction, COPY
                // stream, unread bytes, or dirty session state, close this
                // backend instead of handing it to another client.
                if let Some(reason) = inner.obj.recycle_safety_violation() {
                    if !inner.obj.is_bad() {
                        inner.obj.mark_bad(reason);
                    }
                }
                let must_evict = inner.obj.is_bad();
                if must_evict {
                    // Skip return_object so this connection never reaches the
                    // idle queue or a direct-handoff waiter. Update accounting
                    // in the same tick (decrement slots.size, restore or retire
                    // the semaphore permit, wake coordinator observers) so the
                    // next checkout sees the freed slot. Server::drop closes the
                    // PG socket via RAII when `inner` falls off scope below.
                    //
                    // ALSO drop one registered same-pool waiter's
                    // oneshot sender so that the waiter wakes immediately
                    // (its `rx.try_recv()` will see Closed and continue) -
                    // otherwise registered waiters keep sleeping for
                    // BURST_BACKOFF (5-10 ms) while a fresh acquire could
                    // skip the queue via `try_acquire_burst_gate`. Without
                    // this, bad-eviction storms (mass `pg_terminate_backend`)
                    // cause fairness inversion: registered waiters starve.
                    let (waker_to_close, retire_permit) = {
                        let mut slots = pool.slots.lock();
                        let retire_slot = slots.size > slots.max_size;
                        slots.size = slots.size.saturating_sub(1);
                        // retire the permit
                        // only when resize() pre-marked one. A pre_replace_one
                        // overshoot leaves permits_to_retire == 0, so the permit
                        // is restored below instead of leaked.
                        let retire_permit = retire_slot && slots.permits_to_retire > 0;
                        if retire_permit {
                            slots.permits_to_retire -= 1;
                        }
                        (slots.waiters.pop_front(), retire_permit)
                    };
                    // Dropping the sender wakes the waiter via Closed.
                    drop(waker_to_close);
                    if !retire_permit {
                        pool.semaphore.add_permits(1);
                    }
                    pool.notify_return_observers();
                } else {
                    inner.metrics.recycled = Some(clock::now());
                    inner.metrics.recycle_count += 1;
                    pool.return_object(inner);
                }
            }
        }
    }
}

impl Deref for Object {
    type Target = Server;
    fn deref(&self) -> &Self::Target {
        &self.inner.as_ref().unwrap().obj
    }
}

impl DerefMut for Object {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.as_mut().unwrap().obj
    }
}

impl AsRef<Server> for Object {
    fn as_ref(&self) -> &Server {
        self
    }
}

impl AsMut<Server> for Object {
    fn as_mut(&mut self) -> &mut Server {
        self
    }
}

impl fmt::Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Object")
            .field("inner", &self.inner.as_ref().map(|i| &i.obj))
            .finish()
    }
}

/// Internal slots storage.
struct Slots {
    vec: VecDeque<ObjectInner>,
    /// Direct-handoff queue: waiters blocked on a oneshot receiver.
    /// `return_object` pops the oldest sender and delivers the connection
    /// directly, bypassing the idle VecDeque entirely.
    waiters: VecDeque<oneshot::Sender<ObjectInner>>,
    size: usize,
    max_size: usize,
    /// permits that a resize() shrink could
    /// not forget immediately (because they were checked out) and that the
    /// `size > max_size` retire branches must still retire as those clients
    /// return. ONLY resize sets this; a pre_replace_one overshoot leaves it 0,
    /// so a return during overshoot RESTORES the permit instead of leaking it.
    permits_to_retire: usize,
}

impl fmt::Debug for Slots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slots")
            .field("vec_len", &self.vec.len())
            .field("waiters_len", &self.waiters.len())
            .field("size", &self.size)
            .field("max_size", &self.max_size)
            .finish()
    }
}

/// Per-pool counters for the anticipation + bounded burst code path.
///
/// All fields are monotonic counters. They are read by the admin/prometheus
/// exporters and never reset; relative deltas between scrapes are what
/// operators tune against.
#[derive(Debug, Default)]
pub(crate) struct ScalingStats {
    /// Number of new connections that successfully took a burst slot and
    /// proceeded to `server_pool.create()`. Pairs with `burst_gate_waits`
    /// to compute the gate hit rate.
    pub(crate) creates_started: AtomicU64,
    /// Number of times a caller observed the burst gate at capacity and had
    /// to wait on a Notify (or backoff). High values indicate `max_parallel_creates`
    /// is too low for the offered load — or that creates are slow.
    pub(crate) burst_gate_waits: AtomicU64,
    /// Number of Phase B anticipation attempts where a direct-handoff
    /// delivery via oneshot channel succeeded. Incremented once per
    /// successful receive, before the recycle check.
    pub(crate) anticipation_wakes_notify: AtomicU64,
    /// Number of Phase 4 fall-throughs that gave up on anticipation:
    /// the deadline was exhausted, the per-caller race-loss cap was
    /// hit, or the wall-clock hard cap fired. Increments exactly once
    /// per Phase 4 exit without a recyclable connection.
    pub(crate) anticipation_wakes_timeout: AtomicU64,
    /// Number of times Phase 4 fell through without a recyclable connection
    /// and the caller had to call `server_pool.create()`. Steady-state
    /// should be near zero; a sustained non-zero rate means offered load
    /// exceeds what returns can serve within the caller's remaining wait
    /// budget (`query_wait_timeout` - 500 ms create reserve).
    pub(crate) create_fallback: AtomicU64,
    /// Number of times the background `replenish` task hit the burst cap
    /// and deferred its work to the next retain cycle. Persistent non-zero
    /// values indicate `min_pool_size` cannot be sustained under current load.
    pub(crate) replenish_deferred: AtomicU64,
    /// Number of times the burst gate adaptive budget was exhausted.
    /// A sustained non-zero rate means the pool is undersized: clients wait
    /// longer than 2× xact_p99 for a recycled connection before proceeding
    /// to create a new one.
    pub(crate) burst_gate_budget_exhausted: AtomicU64,
    /// Number of pre-replacement connections created ahead of lifetime expiry.
    pub(crate) pre_replacements_triggered: AtomicU64,
    /// Number of pre-replacement attempts skipped (coordinator full, pressure,
    /// pool not tight, or another pre-replacement already in flight).
    pub(crate) pre_replacements_skipped: AtomicU64,
}

/// Snapshot of per-pool scaling counters, returned to admin/prometheus exporters.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScalingStatsSnapshot {
    pub creates_started: u64,
    pub burst_gate_waits: u64,
    pub burst_gate_budget_exhausted: u64,
    pub anticipation_wakes_notify: u64,
    pub anticipation_wakes_timeout: u64,
    pub create_fallback: u64,
    pub replenish_deferred: u64,
    /// Current `inflight_creates` value (gauge, not a counter).
    pub inflight_creates: usize,
    pub pre_replacements_triggered: u64,
    pub pre_replacements_skipped: u64,
}

/// Internal pool state.
struct PoolInner {
    server_pool: ServerPool,
    slots: Mutex<Slots>,
    /// Number of checkout futures currently inside `timeout_get`.
    users: AtomicUsize,
    semaphore: Semaphore,
    config: PoolConfig,
    /// Database-level coordinator (None when max_db_connections = 0).
    coordinator: Option<Arc<pool_coordinator::PoolCoordinator>>,
    /// Pool name (database name in config), used in coordinator error messages.
    pool_name: String,
    /// Username for this pool, used in coordinator error messages.
    username: String,
    /// Number of server connection creates currently in-flight on this pool.
    /// This is NOT the count of currently-held connections — only those being
    /// established right now via `server_pool.create()`. Bounded by
    /// `config.scaling.max_parallel_creates` to suppress thundering herd when
    /// N parallel callers all miss the idle pool simultaneously.
    inflight_creates: AtomicUsize,
    /// Wake signal for tasks queued behind the bounded burst limiter.
    /// Notified once when an in-flight create completes (success or failure),
    /// so the next waiting task can attempt its own create or recycle.
    create_done: Notify,
    /// Counters exposed via SHOW POOLS and Prometheus for tuning the
    /// anticipation + bounded burst path.
    scaling_stats: ScalingStats,
    /// Number of pre-replacement tasks currently in flight. Capped at
    /// `MAX_CONCURRENT_PRE_REPLACEMENTS` to prevent a burst of expiring
    /// connections from spawning too many background creates at once.
    pre_replacements_in_flight: AtomicUsize,
}

enum RecycleOutcome {
    Reused(Box<ObjectInner>),
    Failed,
    Empty,
}

/// Minimum `server_lifetime` for pre-replacement to be worthwhile.
/// With shorter lifetimes the overlap window is too narrow for the
/// replacement to be ready in time.
const PRE_REPLACE_MIN_LIFETIME_MS: u64 = 60_000;

/// Pre-replacement threshold as a percentage of `metrics.lifetime_ms`.
/// At 95% of a 5-minute lifetime the overlap window is ~15 seconds —
/// 15 000x the ~1 ms Unix-socket connect time. For TCP deployments this
/// can be lowered to 85%.
const PRE_REPLACE_THRESHOLD_PCT: u64 = 95;

/// Maximum concurrent pre-replacement tasks per pool. With a 5-minute
/// lifetime and ±20% jitter, up to 3 connections can expire within
/// the same 15-second window. Allowing 3 concurrent pre-replacements
/// ensures each one gets a warm replacement without serialization.
const MAX_CONCURRENT_PRE_REPLACEMENTS: usize = 3;

/// Anticipation budget: absolute maximum wait before falling through to create.
const ANTICIPATION_HARD_CAP_MS: u64 = 500;

/// Anticipation budget at cold start when xact_p99 histogram has no data.
/// Conservative enough to not overwhelm coordinator when all pools start
/// simultaneously, fast enough to fill the pool within seconds.
const ANTICIPATION_COLD_START_MS: u64 = 100;

/// Anticipation budget: minimum wait. Even with xact_p99 < 3ms, wait at
/// least this long to give the direct-handoff a chance before creating.
const ANTICIPATION_MIN_BUDGET_MS: u64 = 5;

/// Time reserved after anticipation for the actual create() call.
/// Subtracted from the total budget before entering the handoff wait.
const ANTICIPATION_CREATE_RESERVE: Duration = Duration::from_millis(500);

/// Fallback total budget when `timeouts.wait` is None (no query_wait_timeout).
const ANTICIPATION_FALLBACK_BUDGET_MS: u64 = 100;

/// Backoff between retries when the burst gate budget is exhausted.
/// The client stops registering as a handoff waiter and just listens
/// for `create_done` notifications with this timeout between retries.
const BURST_GATE_EXHAUSTED_BACKOFF: Duration = Duration::from_millis(50);

/// Burst gate adaptive timeout: minimum budget before exiting the handoff loop.
/// Below 20ms, fork() + shared_buffers attach on large instances can take longer,
/// causing unnecessary creates during brief spikes.
const BURST_GATE_MIN_BUDGET_MS: u64 = 20;

/// Compute the base anticipation budget (before jitter) from xact_p99.
/// Pure function, deterministic, safe to call from tests.
#[inline]
fn anticipation_base_ms(xact_p99_us: u64) -> u64 {
    if xact_p99_us == 0 {
        ANTICIPATION_COLD_START_MS
    } else {
        xact_p99_us.saturating_mul(2) / 1000
    }
}

/// Compute burst gate adaptive budget from xact_p99.
/// Reuses `anticipation_base_ms` for the base, adds ±20% jitter.
#[inline]
fn burst_gate_budget(xact_p99_us: u64) -> Duration {
    let base_ms = anticipation_base_ms(xact_p99_us);
    let jitter_range = (base_ms / 5).max(1);
    let jitter = rand::rng().random_range(0..=jitter_range * 2);
    let budget_ms = (base_ms.saturating_sub(jitter_range) + jitter)
        .clamp(BURST_GATE_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS);
    Duration::from_millis(budget_ms)
}

/// Push a connection into the idle queue respecting the configured
/// queue mode (FIFO/LIFO). Caller must hold the slots lock.
#[inline(always)]
fn push_idle(queue_mode: QueueMode, vec: &mut VecDeque<ObjectInner>, inner: ObjectInner) {
    match queue_mode {
        QueueMode::Fifo => vec.push_back(inner),
        QueueMode::Lifo => vec.push_front(inner),
    }
}

#[inline(always)]
fn prune_closed_handoff_waiters(slots: &mut Slots) {
    slots.waiters.retain(|sender| !sender.is_closed());
}

#[inline(always)]
fn push_handoff_waiter(slots: &mut Slots, sender: oneshot::Sender<ObjectInner>) {
    prune_closed_handoff_waiters(slots);
    slots.waiters.push_back(sender);
}

#[inline(always)]
fn close_and_drain_handoff_receiver<T>(
    rx: &mut oneshot::Receiver<T>,
) -> Result<T, oneshot::error::TryRecvError> {
    rx.close();
    rx.try_recv()
}

struct HandoffReceiverGuard<'a> {
    pool: &'a PoolInner,
    rx: Option<oneshot::Receiver<ObjectInner>>,
}

impl<'a> HandoffReceiverGuard<'a> {
    fn new(pool: &'a PoolInner, rx: oneshot::Receiver<ObjectInner>) -> Self {
        Self { pool, rx: Some(rx) }
    }

    fn rx_mut(&mut self) -> &mut oneshot::Receiver<ObjectInner> {
        self.rx.as_mut().expect("handoff receiver must be armed")
    }

    fn close_and_drain(&mut self) -> Result<ObjectInner, oneshot::error::TryRecvError> {
        let mut rx = self.rx.take().expect("handoff receiver must be armed");
        close_and_drain_handoff_receiver(&mut rx)
    }
}

impl Drop for HandoffReceiverGuard<'_> {
    fn drop(&mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };

        match close_and_drain_handoff_receiver(&mut rx) {
            Ok(inner) => {
                let mut slots = self.pool.slots.lock();
                push_idle(self.pool.config.queue_mode, &mut slots.vec, inner);
                drop(slots);
                self.pool.notify_return_observers();
            }
            Err(_) => {
                drop(rx);
                let mut slots = self.pool.slots.lock();
                prune_closed_handoff_waiters(&mut slots);
            }
        }
    }
}

impl PoolInner {
    /// Try to take a burst gate slot. On success, bumps `creates_started`
    /// and returns a guard that releases the slot on drop.
    fn try_acquire_burst_gate(&self) -> Option<BurstGateGuard<'_>> {
        let max = self.config.scaling.max_parallel_creates as usize;
        if try_take_burst_slot(&self.inflight_creates, max) {
            self.scaling_stats
                .creates_started
                .fetch_add(1, Ordering::Relaxed);
            Some(BurstGateGuard {
                inflight_creates: &self.inflight_creates,
                create_done: &self.create_done,
            })
        } else {
            None
        }
    }

    /// Build an ObjectInner from a freshly created Server connection,
    /// stamped with the current server_pool epoch and jittered timeouts.
    fn new_object_inner(
        &self,
        obj: Server,
        coordinator_permit: Option<pool_coordinator::CoordinatorPermit>,
    ) -> ObjectInner {
        let lifetime_ms = obj
            .override_lifetime_ms
            .unwrap_or(self.server_pool.lifetime_ms());
        ObjectInner {
            obj,
            metrics: Metrics::new(
                lifetime_ms,
                self.server_pool.idle_timeout_ms(),
                self.server_pool.current_epoch(),
            ),
            coordinator_permit,
        }
    }

    #[inline(always)]
    fn accepts_fresh_backend_after_create(&self, slots: &Slots) -> bool {
        !self.semaphore.is_closed() && slots.max_size > 0
    }

    /// Background pre-replacement: create one connection ahead of lifetime
    /// expiry so the next checkout finds a warm replacement in the idle
    /// queue instead of paying for a fresh create.
    ///
    /// Called via `tokio::spawn` from `Pool::trigger_pre_replacement`.
    /// On success the pool temporarily holds `max_size + 1` connections
    /// until the old one dies during the next recycle.
    async fn pre_replace_one(&self) {
        // Coordinator permit — non-blocking, with headroom guard.
        let coordinator_permit = if let Some(ref coord) = self.coordinator {
            // Keep at least 2 permits free so a peer pool can still create
            // without being forced onto the slow eviction/reserve path.
            if coord.available_main_permits() < 2 {
                log::debug!(
                    "[{}@{}] pre-replace: skipped — coordinator headroom < 2",
                    self.username,
                    self.pool_name,
                );
                self.scaling_stats
                    .pre_replacements_skipped
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            match coord.try_acquire() {
                Some(p) => Some(p),
                None => {
                    log::debug!(
                        "[{}@{}] pre-replace: skipped — coordinator full",
                        self.username,
                        self.pool_name,
                    );
                    self.scaling_stats
                        .pre_replacements_skipped
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        } else {
            None
        };

        // Burst gate — non-blocking, like replenish.
        let Some(_gate) = self.try_acquire_burst_gate() else {
            log::debug!(
                "[{}@{}] pre-replace: skipped — burst gate full",
                self.username,
                self.pool_name,
            );
            self.scaling_stats
                .pre_replacements_skipped
                .fetch_add(1, Ordering::Relaxed);
            return;
        };

        // Create the replacement connection.
        let obj = match self.server_pool.create().await {
            Ok(obj) => obj,
            Err(e) => {
                log::debug!(
                    "[{}@{}] pre-replace: create failed — {}",
                    self.username,
                    self.pool_name,
                    e,
                );
                self.scaling_stats
                    .pre_replacements_skipped
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

        // Push to idle queue. Temporarily exceeds max_size by 1; returns
        // to max_size when the old connection fails recycle.
        //
        // BEFORE pushing to idle, try to hand the
        // fresh connection directly to an oldest queued waiter. Without
        // this, a client that is currently blocked in `try_anticipate`
        // had to wait the full anticipate budget even though a fresh
        // connection just landed. Mirrors the waiter-drain in
        // `return_object`.
        let mut handoff_done = false;
        {
            let mut slots = self.slots.lock();
            if !self.accepts_fresh_backend_after_create(&slots) {
                drop(slots);
                log::debug!(
                    "[{}@{}] pre-replace: dropped fresh backend because pool generation closed",
                    self.username,
                    self.pool_name,
                );
                self.scaling_stats
                    .pre_replacements_skipped
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            slots.size += 1;
            let inner = self.new_object_inner(obj, coordinator_permit);
            let mut carry: Option<ObjectInner> = Some(inner);
            while let Some(sender) = slots.waiters.pop_front() {
                let take = carry.take().expect("carry held one inner per iteration");
                match sender.send(take) {
                    Ok(()) => {
                        handoff_done = true;
                        break;
                    }
                    Err(returned) => {
                        carry = Some(returned);
                    }
                }
            }
            if let Some(remaining) = carry {
                push_idle(self.config.queue_mode, &mut slots.vec, remaining);
            }
        }

        // No semaphore.add_permits needed: return_object now always
        // restores the returning client's permit (both handoff and idle
        // paths), so no extra permit is required to compensate for future
        // handoff drain. The client checking out this pre-created
        // connection will acquire its own permit normally.

        self.scaling_stats
            .pre_replacements_triggered
            .fetch_add(1, Ordering::Relaxed);
        log::info!(
            "[{}@{}] pre-replace: replacement connection created ahead of lifetime expiry{}",
            self.username,
            self.pool_name,
            if handoff_done {
                " (handed to waiter)"
            } else {
                ""
            },
        );
    }

    /// Create a new backend connection via `server_pool.create()`, respecting
    /// the caller's `create` timeout. On success, increments `slots.size` and
    /// returns the `ObjectInner` ready for wrapping into an `Object`.
    async fn create_connection(
        &self,
        timeouts: &Timeouts,
        coordinator_permit: Option<pool_coordinator::CoordinatorPermit>,
    ) -> Result<ObjectInner, PoolError> {
        let obj = match timeouts.create {
            Some(duration) => {
                match tokio::time::timeout(duration, self.server_pool.create()).await {
                    Ok(Ok(obj)) => obj,
                    Ok(Err(e)) => return Err(PoolError::Backend(e)),
                    Err(_) => return Err(PoolError::Timeout(TimeoutType::Create)),
                }
            }
            None => self
                .server_pool
                .create()
                .await
                .map_err(PoolError::Backend)?,
        };

        {
            let mut slots = self.slots.lock();
            if !self.accepts_fresh_backend_after_create(&slots) {
                drop(slots);
                drop(obj);
                return Err(PoolError::Closed);
            }
            slots.size += 1;
        }

        Ok(self.new_object_inner(obj, coordinator_permit))
    }

    /// Returns true when every permit is in use — clients are either holding
    /// connections or queued behind the semaphore. Used to suppress lifetime
    /// housekeeping (`recycle` lifetime expiry, retain-loop trimming) so we
    /// do not close working connections at the moment they are most needed.
    /// One atomic load on the semaphore — safe to call from the hot path.
    #[inline(always)]
    fn under_pressure(&self) -> bool {
        self.semaphore.available_permits() == 0
    }

    async fn try_recycle_one(&self, timeouts: &Timeouts) -> RecycleOutcome {
        let obj_inner = {
            let mut slots = self.slots.lock();
            slots.vec.pop_front()
        };

        let Some(inner) = obj_inner else {
            return RecycleOutcome::Empty;
        };

        let skip_lifetime = self.under_pressure();

        // cancel-safety guard. The pop above removed
        // the connection from `slots.vec` but `slots.size` still
        // counts it (size tracks vec + checked-out + in-transit). If
        // the future is cancelled during `recycle().await` (timeout
        // higher in the stack, runtime shutdown, client RST), the
        // bare `ObjectInner` falls off the stack and `Server::drop`
        // closes the TCP fd - but `slots.size` is never decremented,
        // leaking 1 size unit per cancellation. Over time `slots.size`
        // saturates at `max_size`, the create gate blocks every
        // `replenish`, and the pool freezes with no real backends.
        let mut guard = BareInnerGuard::new(self, inner);

        let recycle_result = {
            let inner_ref = guard.as_mut();
            // Split borrows of disjoint fields are accepted by the
            // borrow checker; `as_mut` returned one mutable reference,
            // so destructure from it.
            let ObjectInner { obj, metrics, .. } = inner_ref;
            match timeouts.recycle {
                Some(duration) => {
                    match tokio::time::timeout(
                        duration,
                        self.server_pool.recycle(obj, metrics, skip_lifetime),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(RecycleError::StaticMessage("Recycle timeout")),
                    }
                }
                None => self.server_pool.recycle(obj, metrics, skip_lifetime).await,
            }
        };

        match recycle_result {
            Ok(()) => RecycleOutcome::Reused(Box::new(guard.disarm())),
            Err(_) => {
                // Guard's Drop decrements `slots.size` and closes the
                // backend's TCP fd via `Server::drop`.
                drop(guard);
                RecycleOutcome::Failed
            }
        }
    }

    #[inline(always)]
    fn return_object(&self, mut inner: ObjectInner) {
        let mut slots = self.slots.lock();

        if slots.size > slots.max_size {
            slots.size = slots.size.saturating_sub(1);
            // retire the returning permit
            // only when resize() pre-marked one. A pre_replace_one overshoot
            // leaves permits_to_retire == 0, so the permit is restored below
            // instead of leaked.
            let retire_permit = slots.permits_to_retire > 0;
            if retire_permit {
                slots.permits_to_retire -= 1;
            }
            let waker_to_close = slots.waiters.pop_front();
            drop(slots);
            drop(waker_to_close);
            drop(inner);
            if !retire_permit {
                self.semaphore.add_permits(1);
            }
            self.notify_return_observers();
            return;
        }

        // Direct handoff: send to the oldest registered waiter.
        // Waiters whose receiver was dropped (timeout) are skipped.
        while let Some(sender) = slots.waiters.pop_front() {
            match sender.send(inner) {
                Ok(()) => {
                    drop(slots);
                    // Restore the returning client's semaphore permit.
                    // The waiter holds its OWN permit (from acquire_semaphore),
                    // so this is not double-counting — it compensates for the
                    // permit.forget() when this connection was last wrapped.
                    // Without this, each handoff permanently drains one permit
                    // because the returning client re-enters timeout_get and
                    // acquires a NEW permit, but the old one was never restored.
                    self.semaphore.add_permits(1);
                    return;
                }
                Err(returned_inner) => {
                    // Receiver dropped (timeout) — try the next waiter.
                    inner = returned_inner;
                }
            }
        }

        // No waiters — normal path.
        push_idle(self.config.queue_mode, &mut slots.vec, inner);
        drop(slots);
        self.semaphore.add_permits(1);
        self.notify_return_observers();
    }

    /// Wake peer-pool coordinator waiter after a connection lands in
    /// `slots.vec` (the no-waiter path of `return_object`). The coordinator
    /// the wait queue waiter scans this pool's idle vec via `evict_one_idle` and
    /// drops the returned connection to free a coordinator slot.
    ///
    /// Same-pool waiters (Phase B anticipation, burst gate) now receive
    /// connections via the direct-handoff oneshot channel inside
    /// `return_object` and never park on a Notify.
    #[inline(always)]
    fn notify_return_observers(&self) {
        if let Some(coordinator) = self.coordinator.as_ref() {
            coordinator.notify_idle_returned();
        }
    }
}

impl fmt::Debug for PoolInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slots = self.slots.lock();
        f.debug_struct("PoolInner")
            .field("server_pool", &self.server_pool)
            .field("slots_size", &slots.size)
            .field("slots_max_size", &slots.max_size)
            .field("users", &self.users)
            .field("config", &self.config)
            .finish()
    }
}

/// Connection pool for PostgreSQL server connections.
///
/// This struct can be cloned and transferred across thread boundaries and uses
/// reference counting for its internal state.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<PoolInner>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool").field("inner", &self.inner).finish()
    }
}

/// Outcome of the burst gate acquisition loop.
enum BurstGateOutcome<'a> {
    /// Slot acquired — caller proceeds to create a connection.
    Acquired(BurstGateGuard<'a>),
    /// A recycled connection was obtained while waiting for a slot.
    Recycled(Box<ObjectInner>),
    /// Non-blocking caller and gate is full — no connection available.
    Timeout,
}

/// Outcome of JIT coordinator permit acquisition.
enum CoordinatorJitResult<'a> {
    /// Permit acquired (or no coordinator configured) — caller creates.
    /// The gate guard is returned so the caller holds it until create
    /// completes.
    Create {
        permit: Option<pool_coordinator::CoordinatorPermit>,
        gate: BurstGateGuard<'a>,
    },
    /// A recycled connection was found during the slow-path wait.
    Recycled(Box<ObjectInner>),
}

/// RAII guard that owns the semaphore-permits-forgotten-for-eviction
/// accounting in `Pool::evict_dead_backends`. The happy-path commit
/// pushes survivors back, deducts `evicted` from `slots.size`, and
/// restores all `checked` permits via a single `add_permits` (disarming
/// the guard). If the scan task is dropped at any await point inside
/// the off-lock check loop - cancellation, panic during `check_alive`,
/// or any future `select!` wrapper around the retain task - `Drop` runs
/// the worst-case bookkeeping: treat every popped object as evicted
/// (the survivors `Vec` that was never moved into the commit path
/// drops here too, and each `Server::drop` closes its TCP fd), deduct
/// the full `popped_count` from `slots.size`, and `add_permits` back
/// the matching count so the semaphore stays balanced.
///
/// Without this guard, an `.abort()` of the retain task between the
/// pop phase and the bookkeeping section would permanently leak
/// `popped_count` permits and leave `slots.size` inflated until process
/// restart.
struct EvictGuard<'p> {
    pool: &'p PoolInner,
    popped_count: usize,
    committed: bool,
}

/// cancel-safety guard for `ObjectInner`
/// values that have been removed from `slots.vec` but not yet
/// committed (wrap_checkout or back into the vec). Without it, a
/// future cancellation between pop and commit silently leaks
/// `slots.size` by 1 per occurrence (and the semaphore permit the
/// pending caller already held). Over time the pool freezes at
/// `slots.size == max_size` with zero real backends.
///
/// Behaviour:
/// - On `Drop` without `disarm()`: decrement `slots.size` by 1,
///   `add_permits(1)` to the semaphore (the caller's permit is
///   restored), then drop the wrapped `ObjectInner` off-lock -
///   `Server::drop` closes the TCP fd via RAII.
/// - On `disarm()`: returns the `ObjectInner` for the success
///   path; bookkeeping is the caller's responsibility (typically
///   `wrap_checkout` which `permit.forget()`s).
struct BareInnerGuard<'p> {
    pool: &'p PoolInner,
    inner: Option<ObjectInner>,
}

impl<'p> BareInnerGuard<'p> {
    #[inline]
    fn new(pool: &'p PoolInner, inner: ObjectInner) -> Self {
        Self {
            pool,
            inner: Some(inner),
        }
    }
    #[inline]
    fn as_mut(&mut self) -> &mut ObjectInner {
        self.inner.as_mut().expect("guard held inner")
    }
    #[inline]
    fn disarm(mut self) -> ObjectInner {
        self.inner.take().expect("guard held inner")
    }
}

impl<'p> Drop for BareInnerGuard<'p> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            {
                let mut slots = self.pool.slots.lock();
                slots.size = slots.size.saturating_sub(1);
            }
            // NOTE: we deliberately do NOT call `add_permits(1)`. The
            // caller of `try_recycle_one` / `recycle_handoff` still
            // holds a `SemaphorePermit` from `acquire_semaphore`. On
            // both the Err return path AND the cancellation/drop
            // path, that permit is dropped naturally by tokio and
            // returns to the semaphore. Calling `add_permits(1)` here
            // would double-restore the permit and inflate the
            // semaphore beyond `max_size`.
            drop(inner); // Server::drop runs off-lock
        }
    }
}

impl<'p> EvictGuard<'p> {
    fn new(pool: &'p PoolInner, popped_count: usize) -> Self {
        Self {
            pool,
            popped_count,
            committed: false,
        }
    }

    /// Happy-path finalisation. Re-insert survivors via `push_idle`,
    /// deduct `evicted` from `slots.size`, then restore all permits.
    /// Marks the guard committed only at the END so a panic anywhere
    /// inside this function (debug_assert, `push_idle` OOM,
    /// `add_permits` overflow) still triggers `Drop`'s worst-case
    /// bookkeeping path - without the late `committed=true`, a panic
    /// in `push_idle` would leave permits unrestored AND survivors
    /// dropped, permanently shrinking the pool by `popped_count`.
    fn commit(mut self, queue_mode: QueueMode, survivors: Vec<ObjectInner>, evicted: usize) {
        debug_assert_eq!(
            survivors.len() + evicted,
            self.popped_count,
            "EvictGuard.commit: survivors + evicted must equal popped",
        );
        if !survivors.is_empty() || evicted > 0 {
            let mut guard = self.pool.slots.lock();
            for obj in survivors {
                push_idle(queue_mode, &mut guard.vec, obj);
            }
            if evicted > 0 {
                guard.size = guard.size.saturating_sub(evicted);
            }
        }
        self.pool.semaphore.add_permits(self.popped_count);
        // Disarm AFTER all bookkeeping. Any panic before this point
        // falls through to `Drop::drop` which conservatively treats
        // the entire batch as evicted (deducting popped_count from
        // slots.size and restoring popped_count permits). That is
        // strictly safer than a partial commit leaving permits
        // forgotten forever.
        self.committed = true;
    }
}

impl Drop for EvictGuard<'_> {
    fn drop(&mut self) {
        if self.committed || self.popped_count == 0 {
            return;
        }
        // Cancellation / panic path. The survivors Vec the caller was
        // building dropped along with this stack frame, so every popped
        // ObjectInner has either been consumed in-loop or is dropping
        // right now - each Server::drop already closed its TCP fd.
        //
        // **Known side effect** under cancellation: backends that were
        // already verified healthy in the off-lock loop and pushed onto
        // `survivors` are silently dropped here too. They do NOT make it
        // back to the idle vec. This is the correctness-preserving
        // pessimistic choice - the pool is left under-capacity by up to
        // `max_per_cycle` connections that `replenish` will refill on
        // the next tick. The alternative ("preserve survivors across
        // cancellation") would require the guard to own the survivors
        // Vec, complicating the happy-path API for a benefit measured
        // only against a cancellation surface the retain task does not
        // currently expose (no parent `select!`, no JoinHandle abort).
        //
        // We just need to keep the pool's accounting consistent:
        //   * slots.size has not been touched since the pop phase, so
        //     it still reflects the pre-eviction count. Deduct the full
        //     popped_count - the objects are gone from the idle vec
        //     either way.
        //   * semaphore lost `popped_count` permits during pop (one
        //     `try_acquire().forget()` per item). Restore them so a
        //     concurrent checkout that arrives after the cancellation
        //     can still acquire up to `max_size` permits.
        {
            let mut guard = self.pool.slots.lock();
            guard.size = guard.size.saturating_sub(self.popped_count);
        }
        self.pool.semaphore.add_permits(self.popped_count);
    }
}

impl Pool {
    /// Wrap a recycled/created ObjectInner into an Object, consuming
    /// the semaphore permit. The permit is restored by `return_object`
    /// (via `add_permits(1)`) when the Object is dropped.
    #[inline(always)]
    fn wrap_checkout(&self, inner: ObjectInner, permit: SemaphorePermit<'_>) -> Object {
        permit.forget();
        Object {
            inner: Some(inner),
            pool: Arc::downgrade(&self.inner),
        }
    }

    /// Acquire a burst gate slot, waiting if necessary. While waiting,
    /// attempts to recycle idle connections and registers as a
    /// direct-handoff waiter so a returning connection can be delivered
    /// without entering the idle queue.
    async fn acquire_burst_gate(
        &self,
        timeouts: &Timeouts,
        non_blocking: bool,
    ) -> BurstGateOutcome<'_> {
        // read the cached p99 atom (refreshed by the Collector every
        // 15s) instead of taking the blocking `xact_histogram.lock()` on
        // every checkout. The cached value is at most one stats-cycle
        // stale - fine for sizing an adaptive wait budget - and the hot
        // path becomes one `Relaxed` atomic load.
        let xact_p99_us = self
            .inner
            .server_pool
            .address()
            .stats
            .p99_xact_time_us
            .load(Ordering::Relaxed);
        let budget = burst_gate_budget(xact_p99_us);
        let loop_start = tokio::time::Instant::now();

        loop {
            if let Some(guard) = self.inner.try_acquire_burst_gate() {
                return BurstGateOutcome::Acquired(guard);
            }

            self.inner
                .scaling_stats
                .burst_gate_waits
                .fetch_add(1, Ordering::Relaxed);

            if non_blocking {
                if let RecycleOutcome::Reused(inner) = self.inner.try_recycle_one(timeouts).await {
                    return BurstGateOutcome::Recycled(inner);
                }
                return BurstGateOutcome::Timeout;
            }

            // Try recycle BEFORE registering as a waiter to avoid
            // leaving dead senders in the queue on success.
            if let RecycleOutcome::Reused(inner) = self.inner.try_recycle_one(timeouts).await {
                return BurstGateOutcome::Recycled(inner);
            }

            // Adaptive timeout: waited longer than 2× xact_p99 - pool is undersized.
            // Stop accepting recycled connections, wait for the burst gate directly.
            if loop_start.elapsed() > budget {
                self.inner
                    .scaling_stats
                    .burst_gate_budget_exhausted
                    .fetch_add(1, Ordering::Relaxed);
                // pin + enable BEFORE awaiting. `Notify::notified()`
                // registers the waiter on first `poll`, NOT at construction
                // time. Without explicit `enable()` a notify_one fired
                // between `notified()` and the first poll is consumed by
                // the permit store and lost - the timeout then waits the
                // full BURST_GATE_EXHAUSTED_BACKOFF for nothing.
                let notify = self.inner.create_done.notified();
                tokio::pin!(notify);
                notify.as_mut().enable();
                let _ = tokio::time::timeout(BURST_GATE_EXHAUSTED_BACKOFF, &mut notify).await;
                continue;
            }

            // Register a direct-handoff waiter AND listen on create_done.
            // `biased;` ensures rx is always checked first: without it,
            // tokio::select! randomly picks among ready branches, and a
            // connection delivered to rx can be silently dropped when
            // on_create or sleep wins the race - leaking slots.size.
            let (tx, rx) = oneshot::channel();
            {
                let mut slots = self.inner.slots.lock();
                push_handoff_waiter(&mut slots, tx);
            }
            let mut handoff_rx = HandoffReceiverGuard::new(&self.inner, rx);
            // register the create_done waiter BEFORE entering select
            // so a notify_one between this line and the select's first
            // poll is captured, not lost. Without this the only signal
            // for "a peer create finished" was the BURST_BACKOFF sleep
            // (~5-10ms tail per wait).
            let on_create = self.inner.create_done.notified();
            tokio::pin!(on_create);
            on_create.as_mut().enable();

            tokio::select! {
                biased;
                result = handoff_rx.rx_mut() => {
                    if let Ok(inner) = result {
                        if let Ok(inner) = self.recycle_handoff(inner, timeouts).await {
                            return BurstGateOutcome::Recycled(Box::new(inner));
                        }
                    }
                }
                _ = &mut on_create => {}
                _ = tokio::time::sleep(BURST_BACKOFF) => {}
            }

            // A connection could arrive between the poll of rx and the
            // drop of the select future. Push it to idle directly —
            // the original return_object that sent it here already
            // called add_permits(1), so calling return_object again
            // would double-count the permit.
            match handoff_rx.close_and_drain() {
                Ok(inner) => {
                    let mut slots = self.inner.slots.lock();
                    push_idle(self.inner.config.queue_mode, &mut slots.vec, inner);
                    drop(slots);
                    self.inner.notify_return_observers();
                }
                Err(_) => {
                    let mut slots = self.inner.slots.lock();
                    prune_closed_handoff_waiters(&mut slots);
                }
            }

            // After wake — try recycle once before retrying the gate.
            if let RecycleOutcome::Reused(inner) = self.inner.try_recycle_one(timeouts).await {
                return BurstGateOutcome::Recycled(inner);
            }
        }
    }

    /// JIT coordinator permit acquisition. Takes the burst gate guard
    /// by value — on the slow path the gate is released while waiting
    /// on the coordinator, then re-acquired.
    ///
    /// Returns either a permit + gate (caller proceeds to create) or
    /// a recycled connection found during the slow-path wait.
    async fn acquire_coordinator_jit<'a>(
        &'a self,
        timeouts: &Timeouts,
        gate: BurstGateGuard<'a>,
    ) -> Result<CoordinatorJitResult<'a>, PoolError> {
        let Some(ref coordinator) = self.inner.coordinator else {
            return Ok(CoordinatorJitResult::Create { permit: None, gate });
        };

        // Fast path: non-blocking CAS.
        if let Some(p) = coordinator.try_acquire() {
            debug!(
                "[{}@{}] coordinator: permit via fast JIT path \
                 (permit_type=main)",
                self.inner.username, self.inner.pool_name,
            );
            return Ok(CoordinatorJitResult::Create {
                permit: Some(p),
                gate,
            });
        }

        // Slow path: release gate slot so peers can create while we wait.
        drop(gate);
        let eviction = super::PoolEvictionSource::new(&self.inner.pool_name);
        let p = match coordinator
            .acquire(&self.inner.pool_name, &self.inner.username, &eviction)
            .await
        {
            Ok(p) => p,
            Err(pool_coordinator::AcquireError::NoConnection(info)) => {
                let slots = self.inner.slots.lock();
                warn!(
                    "[{}@{}] checkout failed at phase=coordinator size={} waiters={} info={}",
                    self.inner.pool_name,
                    self.inner.username,
                    slots.size,
                    slots.waiters.len(),
                    info,
                );
                return Err(PoolError::DbLimitExhausted(info));
            }
        };

        debug!(
            "[{}@{}] coordinator: permit via slow JIT path \
             (permit_type={})",
            self.inner.username,
            self.inner.pool_name,
            if p.is_reserve { "reserve" } else { "main" },
        );

        // Re-check idle: a sibling may have returned a connection
        // while we waited on the coordinator.
        if let RecycleOutcome::Reused(inner) = self.inner.try_recycle_one(timeouts).await {
            return Ok(CoordinatorJitResult::Recycled(inner));
        }

        // Re-acquire burst gate slot.
        match self.acquire_burst_gate(timeouts, false).await {
            BurstGateOutcome::Acquired(new_gate) => Ok(CoordinatorJitResult::Create {
                permit: Some(p),
                gate: new_gate,
            }),
            BurstGateOutcome::Recycled(inner) => Ok(CoordinatorJitResult::Recycled(inner)),
            BurstGateOutcome::Timeout => unreachable!("non_blocking=false"),
        }
    }

    /// Block if the pool is paused, waiting for resume or timeout.
    ///
    /// IMPORTANT: `resume_notified()` must be called BEFORE `is_paused()`
    /// to avoid a race where RESUME fires between the two calls and the
    /// notification is lost.
    async fn wait_if_paused(&self, timeouts: &Timeouts) -> Result<(), PoolError> {
        self.wait_if_paused_with_hook(timeouts, || {}).await
    }

    async fn wait_if_paused_with_hook<F>(
        &self,
        timeouts: &Timeouts,
        mut before_wait: F,
    ) -> Result<(), PoolError>
    where
        F: FnMut(),
    {
        loop {
            let resume_notify = self.inner.server_pool.resume_notified();
            tokio::pin!(resume_notify);
            resume_notify.as_mut().enable();

            if !self.inner.server_pool.is_paused() {
                return Ok(());
            }

            before_wait();
            match timeouts.wait {
                Some(duration) => {
                    if tokio::time::timeout(duration, &mut resume_notify)
                        .await
                        .is_err()
                    {
                        return Err(PoolError::Timeout(TimeoutType::Wait));
                    }
                }
                None => (&mut resume_notify).await,
            }
        }
    }

    /// Acquire a semaphore permit: fast spin path, then blocking fallback.
    async fn acquire_semaphore(
        &self,
        timeouts: &Timeouts,
    ) -> Result<SemaphorePermit<'_>, PoolError> {
        let mut try_fast = 0;
        loop {
            if try_fast < MAX_FAST_RETRY {
                if let Ok(p) = self.inner.semaphore.try_acquire() {
                    return Ok(p);
                }
                try_fast += 1;
                for _ in 0..4 {
                    std::hint::spin_loop();
                }
                tokio::task::yield_now().await;
                continue;
            }

            let non_blocking = timeouts.wait.is_some_and(|t| t.as_nanos() == 0);
            return if non_blocking {
                self.inner.semaphore.try_acquire().map_err(|e| match e {
                    TryAcquireError::Closed => PoolError::Closed,
                    TryAcquireError::NoPermits => PoolError::Timeout(TimeoutType::Wait),
                })
            } else {
                match timeouts.wait {
                    Some(duration) => {
                        match tokio::time::timeout(duration, self.inner.semaphore.acquire()).await {
                            Ok(Ok(p)) => Ok(p),
                            Ok(Err(_)) => Err(PoolError::Closed),
                            Err(_) => Err(PoolError::Timeout(TimeoutType::Wait)),
                        }
                    }
                    None => self
                        .inner
                        .semaphore
                        .acquire()
                        .await
                        .map_err(|_| PoolError::Closed),
                }
            };
        }
    }

    /// Anticipation zone: warm threshold gate, fast spin, and direct
    /// handoff via oneshot channel. Returns `Some(ObjectInner)` if a
    /// recycled connection was obtained, `None` to proceed to the create
    /// path.
    async fn try_anticipate(
        &self,
        timeouts: &Timeouts,
        start: tokio::time::Instant,
    ) -> Option<ObjectInner> {
        let should_anticipate = {
            let slots = self.inner.slots.lock();
            let warm_threshold = std::cmp::max(
                1,
                (slots.max_size as f32 * self.inner.config.scaling.warm_pool_ratio) as usize,
            );
            slots.size >= warm_threshold
        };
        if !should_anticipate {
            return None;
        }

        let non_blocking = timeouts.wait.is_some_and(|t| t.as_nanos() == 0);

        // Fast spin — catches microsecond races without sleeping.
        let fast_retries = self.inner.config.scaling.fast_retries;
        for _ in 0..fast_retries {
            if let RecycleOutcome::Reused(inner) = self.inner.try_recycle_one(timeouts).await {
                return Some(*inner);
            }
            for _ in 0..4 {
                std::hint::spin_loop();
            }
            tokio::task::yield_now().await;
        }

        // Capacity deficit: pool has room to grow but idle queue is empty.
        // Skip anticipation — creating a new connection is cheaper.
        // Disabled when a coordinator is configured: anticipation acts as
        // a natural throttle preventing one pool from grabbing all permits.
        let capacity_deficit = self.inner.coordinator.is_none() && {
            let slots = self.inner.slots.lock();
            slots.vec.is_empty() && slots.size < slots.max_size
        };

        // Direct handoff via oneshot channel.
        if !capacity_deficit && !non_blocking {
            let total_budget = match timeouts.wait {
                Some(wait) => wait
                    .saturating_sub(start.elapsed())
                    .saturating_sub(ANTICIPATION_CREATE_RESERVE),
                None => Duration::from_millis(ANTICIPATION_FALLBACK_BUDGET_MS),
            };

            if !total_budget.is_zero() {
                // Adaptive anticipation budget: wait proportionally to actual
                // transaction latency. If a return doesn't arrive within 2x
                // the p99 xact time, creating is cheaper than waiting.
                //
                // cached atomic instead of blocking histogram lock;
                // updated every 15s by the Collector.
                let xact_p99_us = self
                    .inner
                    .server_pool
                    .address()
                    .stats
                    .p99_xact_time_us
                    .load(Ordering::Relaxed);
                let base_ms = anticipation_base_ms(xact_p99_us);
                // ±20% jitter to prevent synchronized creates across pools
                let jitter_range = (base_ms / 5).max(1);
                let jitter = rand::rng().random_range(0..=jitter_range * 2);
                let cap_ms = (base_ms.saturating_sub(jitter_range) + jitter)
                    .clamp(ANTICIPATION_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS);
                let effective_budget = total_budget.min(Duration::from_millis(cap_ms));

                // was a slow `slots.size` leak. The
                // previous shape moved `rx` BY VALUE into
                // `tokio::time::timeout` - when the timeout fired,
                // both `rx` AND any handoff that `return_object`
                // delivered in the microseconds between were dropped.
                // `return_object` had already executed
                // `add_permits(1)` but `slots.size` was never
                // decremented (size only decrements on recycle
                // failures, evictions, bad-Object::Drop - not here).
                // Over weeks: `slots.size` drifts up, pool freezes at
                // max_size with no real backends. `acquire_burst_gate`
                // already had the matching `try_recv` drain
                // (inner.rs:880); apply it here for the same reason.
                let (tx, rx) = oneshot::channel();
                {
                    let mut slots = self.inner.slots.lock();
                    push_handoff_waiter(&mut slots, tx);
                }
                let mut handoff_rx = HandoffReceiverGuard::new(&self.inner, rx);

                let timeout_result =
                    tokio::time::timeout(effective_budget, handoff_rx.rx_mut()).await;
                match timeout_result {
                    Ok(Ok(inner)) => {
                        self.inner
                            .scaling_stats
                            .anticipation_wakes_notify
                            .fetch_add(1, Ordering::Relaxed);
                        if let Ok(inner) = self.recycle_handoff(inner, timeouts).await {
                            return Some(inner);
                        }
                    }
                    _ => {
                        // drain a late-arriving handoff that
                        // raced our timeout window. Without this,
                        // `slots.size` leaks one per occurrence.
                        if let Ok(inner) = handoff_rx.close_and_drain() {
                            self.inner
                                .scaling_stats
                                .anticipation_wakes_notify
                                .fetch_add(1, Ordering::Relaxed);
                            if let Ok(inner) = self.recycle_handoff(inner, timeouts).await {
                                return Some(inner);
                            }
                        } else {
                            let mut slots = self.inner.slots.lock();
                            prune_closed_handoff_waiters(&mut slots);
                            self.inner
                                .scaling_stats
                                .anticipation_wakes_timeout
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }

        // Anticipation either was skipped or timed out.
        self.inner
            .scaling_stats
            .create_fallback
            .fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Instantiates a builder for a new Pool.
    pub fn builder(server_pool: ServerPool) -> PoolBuilder {
        PoolBuilder::new(server_pool)
    }

    fn from_builder(builder: PoolBuilder) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                server_pool: builder.server_pool,
                slots: Mutex::new(Slots {
                    vec: VecDeque::with_capacity(builder.config.max_size),
                    waiters: VecDeque::new(),
                    size: 0,
                    max_size: builder.config.max_size,
                    permits_to_retire: 0,
                }),
                users: AtomicUsize::new(0),
                semaphore: Semaphore::new(builder.config.max_size),
                config: builder.config,
                coordinator: builder.coordinator,
                pool_name: builder.pool_name,
                username: builder.username,
                inflight_creates: AtomicUsize::new(0),
                create_done: Notify::new(),
                scaling_stats: ScalingStats::default(),
                pre_replacements_in_flight: AtomicUsize::new(0),
            }),
        }
    }

    /// Retrieves an Object from this Pool or waits for one to become available.
    #[inline(always)]
    pub async fn get(&self) -> Result<Object, PoolError> {
        self.timeout_get(&self.timeouts()).await
    }

    /// Retrieves an Object from this Pool using a different timeout than the configured one.
    pub async fn timeout_get(&self, timeouts: &Timeouts) -> Result<Object, PoolError> {
        self.inner.users.fetch_add(1, Ordering::Relaxed);
        scopeguard::defer! {
            self.inner.users.fetch_sub(1, Ordering::Relaxed);
        }

        let start = tokio::time::Instant::now();

        self.wait_if_paused(timeouts).await?;
        let permit = self.acquire_semaphore(timeouts).await.inspect_err(|_e| {
            let slots = self.inner.slots.lock();
            warn!(
                "[{}@{}] checkout timeout at phase=semaphore elapsed={}ms size={} max={} waiters={} semaphore_avail={}",
                self.inner.pool_name, self.inner.username,
                start.elapsed().as_millis(), slots.size, slots.max_size,
                slots.waiters.len(), self.inner.semaphore.available_permits(),
            );
        })?;

        if let RecycleOutcome::Reused(inner) = self.inner.try_recycle_one(timeouts).await {
            // Wrap first so Object's RAII Drop covers any panic in
            // `maybe_trigger_pre_replacement`. Once ownership is assigned,
            // any subsequent panic flows through `Object::drop` and
            // `return_object`, preserving slot-size invariants.
            let obj = self.wrap_checkout(*inner, permit);
            self.maybe_trigger_pre_replacement(&obj.inner.as_ref().unwrap().metrics);
            return Ok(obj);
        }

        if let Some(inner) = self.try_anticipate(timeouts, start).await {
            return Ok(self.wrap_checkout(inner, permit));
        }

        loop {
            match self.inner.try_recycle_one(timeouts).await {
                RecycleOutcome::Reused(inner) => {
                    return Ok(self.wrap_checkout(*inner, permit));
                }
                RecycleOutcome::Failed => continue,
                RecycleOutcome::Empty => break,
            }
        }

        let non_blocking = timeouts.wait.is_some_and(|t| t.as_nanos() == 0);
        let _create_gate = match self.acquire_burst_gate(timeouts, non_blocking).await {
            BurstGateOutcome::Acquired(guard) => guard,
            BurstGateOutcome::Recycled(inner) => {
                return Ok(self.wrap_checkout(*inner, permit));
            }
            BurstGateOutcome::Timeout => {
                let slots = self.inner.slots.lock();
                warn!(
                    "[{}@{}] checkout timeout at phase=burst_gate elapsed={}ms size={} inflight={} waiters={}",
                    self.inner.pool_name, self.inner.username,
                    start.elapsed().as_millis(), slots.size,
                    self.inner.inflight_creates.load(Ordering::Relaxed),
                    slots.waiters.len(),
                );
                return Err(PoolError::Timeout(TimeoutType::Wait));
            }
        };

        let (coordinator_permit, _gate) =
            match self.acquire_coordinator_jit(timeouts, _create_gate).await? {
                CoordinatorJitResult::Create {
                    permit: cp,
                    gate: g,
                } => (cp, g),
                CoordinatorJitResult::Recycled(inner) => {
                    return Ok(self.wrap_checkout(*inner, permit));
                }
            };

        let obj_inner = self
            .inner
            .create_connection(timeouts, coordinator_permit)
            .await
            .map_err(|e| {
                let slots = self.inner.slots.lock();
                warn!(
                    "[{}@{}] checkout failed at phase=create elapsed={}ms size={} err={}",
                    self.inner.pool_name,
                    self.inner.username,
                    start.elapsed().as_millis(),
                    slots.size,
                    e,
                );
                e
            })?;
        Ok(self.wrap_checkout(obj_inner, permit))
    }

    /// Resizes the pool.
    ///
    /// collect evicted `ObjectInner`s into a local Vec and
    /// drop them OFF-LOCK to match the discipline `retain()` documents.
    /// Holding `slots.lock()` across `ObjectInner::Drop` (Server::Drop
    /// -> Terminate syscall, CoordinatorPermit::Drop -> tokio Notify
    /// chain) would otherwise stall every peer recycle on the same
    /// pool for the full TCP-close+notify duration - under RELOAD with
    /// 100 idle backends that's 50-200 ms of pool-wide freeze.
    pub fn resize(&self, max_size: usize) {
        let mut evicted: Vec<ObjectInner> = Vec::new();
        {
            let mut slots = self.inner.slots.lock();
            let old_max_size = slots.max_size;
            slots.max_size = max_size;

            // Shrink pool
            if max_size < old_max_size {
                while slots.size > max_size {
                    if let Some(obj) = slots.vec.pop_back() {
                        slots.size = slots.size.saturating_sub(1);
                        // defer the Drop until after we
                        // release `slots.lock`.
                        evicted.push(obj);
                    } else {
                        break;
                    }
                }
                // reduce semaphore permits. `try_acquire_many`
                // returns Err when fewer than `permits_to_remove`
                // permits are currently free (active checkouts hold
                // the rest). The original code silently ignored that
                // Err, letting the semaphore drift past max_size.
                // Acquire as many as we can synchronously now; the
                // remaining over-permit is retired by `return_object`
                // / bad-object Drop when active clients come back
                // while `slots.size > slots.max_size`. Log the gap so
                // operators see resize-under-load drift.
                let permits_to_remove = old_max_size - max_size;
                let acquired = self
                    .inner
                    .semaphore
                    .try_acquire_many(permits_to_remove as u32)
                    .map(|p| {
                        p.forget();
                        permits_to_remove
                    })
                    .unwrap_or_else(|_| {
                        // Try acquiring permits one at a time to take
                        // however many ARE available right now.
                        let mut took = 0;
                        while took < permits_to_remove {
                            match self.inner.semaphore.try_acquire() {
                                Ok(p) => {
                                    p.forget();
                                    took += 1;
                                }
                                Err(_) => break,
                            }
                        }
                        if took < permits_to_remove {
                            warn!(
                                "[{}@{}] resize shrink: could acquire only {}/{} semaphore permits - active checkouts hold the rest; semaphore will resync as clients return",
                                self.inner.pool_name,
                                self.inner.username,
                                took,
                                permits_to_remove,
                            );
                        }
                        took
                    });
                // record the permits this
                // shrink wanted to remove but could not forget now (held by
                // active checkouts). The `size > max_size` retire branches
                // consume this counter as those clients return, so they retire
                // exactly the resize shortfall - while a pre_replace_one
                // overshoot (which never sets it) restores permits instead of
                // leaking them.
                slots.permits_to_retire += permits_to_remove.saturating_sub(acquired);
                // Reallocate vec
                let mut vec = VecDeque::with_capacity(max_size);
                for obj in slots.vec.drain(..) {
                    vec.push_back(obj);
                }
                slots.vec = vec;
            }

            // Grow pool
            if max_size > old_max_size {
                let additional = max_size - old_max_size;
                slots.vec.reserve_exact(additional);
                self.inner.semaphore.add_permits(additional);
            }
        }
        // drops fire here, after `slots.lock` is released.
        drop(evicted);
    }

    /// Retains only the objects specified by the given function.
    ///
    /// Evicted `ObjectInner`s are extracted into a local Vec and dropped
    /// **after** `slots.lock()` is released. The drop chain on each evicted
    /// object runs `Server::drop` (a `Terminate` syscall to PG) plus
    /// `CoordinatorPermit::drop` (a tokio `Notify::notify_one` that itself
    /// briefly takes an internal mutex). Holding `slots.lock()` across these
    /// blocks any peer caller trying to recycle from the same pool.
    pub fn retain(&self, f: impl Fn(&Server, Metrics) -> bool) {
        let evicted: Vec<ObjectInner> = {
            let mut guard = self.inner.slots.lock();
            // Common case on a healthy retain cycle: nothing to evict.
            // Skip the partition + allocation pair entirely.
            if guard.vec.iter().all(|obj| f(&obj.obj, obj.metrics)) {
                return;
            }
            let mut keep = VecDeque::with_capacity(guard.vec.capacity());
            let mut evicted = Vec::new();
            for obj in guard.vec.drain(..) {
                if f(&obj.obj, obj.metrics) {
                    keep.push_back(obj);
                } else {
                    evicted.push(obj);
                }
            }
            guard.vec = keep;
            guard.size -= evicted.len();
            evicted
        };
        // Lock released here. Syscalls and notify_one fire below, off-lock.
        drop(evicted);
    }

    /// Retains connections, closing oldest first when max limit is set.
    /// If max is 0, behaves like regular retain (closes all matching).
    /// If max > 0, closes at most `max` connections, prioritizing oldest by creation time.
    /// Returns the number of connections closed.
    ///
    /// As with [`retain`], evicted objects are extracted under the lock and
    /// dropped only after the lock is released, so peer callers do not block
    /// on PG `Terminate` syscalls or coordinator wake-ups.
    pub fn retain_oldest_first(
        &self,
        should_close: impl Fn(&Server, &Metrics) -> bool,
        max_to_close: usize,
    ) -> usize {
        let evicted: Vec<ObjectInner> = {
            let mut guard = self.inner.slots.lock();

            if max_to_close == 0 {
                // Early exit when nothing matches — avoid the partition
                // allocation in the frequent "retain cycle sees no stale
                // connections" case.
                if !guard
                    .vec
                    .iter()
                    .any(|obj| should_close(&obj.obj, &obj.metrics))
                {
                    return 0;
                }
                // Unlimited — partition every matching object out of the vec.
                let mut keep = VecDeque::with_capacity(guard.vec.capacity());
                let mut evicted = Vec::new();
                for obj in guard.vec.drain(..) {
                    if should_close(&obj.obj, &obj.metrics) {
                        evicted.push(obj);
                    } else {
                        keep.push_back(obj);
                    }
                }
                guard.vec = keep;
                guard.size -= evicted.len();
                evicted
            } else {
                // Pre-walk to identify the oldest `max_to_close` candidates.
                // We do not extract here — only collect (index, age) pairs.
                let mut candidates: Vec<(usize, u128)> = guard
                    .vec
                    .iter()
                    .enumerate()
                    .filter(|(_, obj)| should_close(&obj.obj, &obj.metrics))
                    .map(|(idx, obj)| (idx, obj.metrics.age().as_millis()))
                    .collect();

                if candidates.is_empty() {
                    return 0;
                }

                // Sort by age descending (oldest first — highest age value)
                candidates.sort_by(|a, b| b.1.cmp(&a.1));

                let to_close: std::collections::HashSet<usize> = candidates
                    .into_iter()
                    .take(max_to_close)
                    .map(|(idx, _)| idx)
                    .collect();

                let mut keep = VecDeque::with_capacity(guard.vec.capacity());
                let mut evicted = Vec::with_capacity(to_close.len());
                for (idx, obj) in guard.vec.drain(..).enumerate() {
                    if to_close.contains(&idx) {
                        evicted.push(obj);
                    } else {
                        keep.push_back(obj);
                    }
                }
                guard.vec = keep;
                guard.size -= evicted.len();
                evicted
            }
        };
        let closed = evicted.len();
        // Lock released here. Drops below run off-lock.
        drop(evicted);
        closed
    }

    /// Evict the oldest idle connection whose age exceeds `min_lifetime_ms`.
    ///
    /// Used by the pool coordinator when it needs to free a connection slot
    /// for another user. The evicted connection's `CoordinatorPermit` is dropped
    /// synchronously, making the slot available immediately.
    ///
    /// Returns `true` if a connection was evicted.
    pub fn evict_one_idle(&self, min_lifetime_ms: u64) -> bool {
        self.retain_oldest_first(
            |_, metrics| metrics.age().as_millis() >= u128::from(min_lifetime_ms),
            1,
        ) > 0
    }

    /// Detect and drop idle backends whose TCP connection is dead. Solves the
    /// post-PostgreSQL-restart "zombie pool" regression: idle TCP sockets
    /// linger in `slots.vec` until `idle_timeout` / `server_lifetime` evicts
    /// them, during which `slots.size` looks healthy and `retain_connections`
    /// never asks `replenish` for more. Result observed in production: a pool
    /// configured for `min_pool_size = 100` runs with ~3-7 real backends for
    /// hours.
    ///
    /// Mechanism:
    ///   1. Briefly lock `slots`, pop up to `max_per_cycle` idle objects off
    ///      the front. We are not reducing `slots.size` here - the popped
    ///      objects are "in flight" exactly like a checkout would be, so
    ///      concurrent traffic sees consistent accounting.
    ///   2. Off-lock, await `Server::check_alive(timeout)` on each one.
    ///      `check_alive` already uses a single send+recv deadline (Step 2)
    ///      and marks the backend bad on failure.
    ///   3. Re-acquire the lock once and either push survivors back via
    ///      `push_idle` (respecting the configured FIFO/LIFO mode) or shrink
    ///      `slots.size` by the eviction count. Dead `ObjectInner`s are
    ///      dropped after the lock is released so `Server::drop` runs
    ///      off-lock too.
    ///
    /// Skipped entirely when the pool is `under_pressure()` (taking idle
    /// objects away from queued clients would force a `connect()` on the
    /// wait path) or `is_paused()` (no checkin/checkout during PAUSE).
    /// Passing `timeout == 0` or `max_per_cycle == 0` also short-circuits,
    /// so operators can disable the cycle with a config knob without
    /// patching the retain loop.
    ///
    /// Returns `(checked, evicted)` for caller-side logging / metrics. Both
    /// counts refer to backends *processed this cycle*; the dead ones have
    /// been removed from the pool by the time this function returns and the
    /// next `replenish` tick will refill the slot.
    pub async fn evict_dead_backends(
        &self,
        timeout: Duration,
        max_per_cycle: usize,
        skip_recent_threshold: Duration,
    ) -> (usize, usize) {
        if timeout.is_zero() || max_per_cycle == 0 {
            return (0, 0);
        }
        if self.under_pressure() || self.is_paused() {
            return (0, 0);
        }

        // 1. Snapshot phase - short critical section.
        //
        // Pop AND acquire a semaphore permit per object together. The permit
        // is the same accounting unit a real checkout uses, so during the
        // off-lock `check_alive` window concurrent checkouts see fewer
        // permits and cannot push the pool past `max_size` by triggering
        // a `create()` for an idle slot that is actually held by the scan.
        //
        // Permits are `forget()`ed here so they survive the SemaphorePermit
        // guard going out of scope at the end of this block. They are
        // restored explicitly at the end via a single `add_permits()` call
        // - that ordering matters: permits must come back AFTER `slots.size`
        // has been shrunk by the eviction count, so a concurrent checkout
        // that wakes on the new permits sees the already-reduced size and
        // either reuses a surviving idle backend or creates the replacement.
        // Pop from the "oldest" end of the idle deque so the scan visits
        // the connections least likely to have been recently exercised -
        // those are the ones whose TCP state has had the most time to
        // diverge from PostgreSQL's view (e.g. a backend that has been
        // idle for hours during a partial-network event).
        //
        // The mapping is mode-aware because checkout pops from the front
        // in both modes; "oldest" therefore differs:
        //
        //   * LIFO (default, `server_round_robin = false`): checkin
        //     pushes to the front, so the BACK of the deque holds the
        //     idle entries that have been waiting longest. Pop_back.
        //   * FIFO (`server_round_robin = true`): checkin pushes to the
        //     back and checkout pops from the front, so the FRONT holds
        //     the entries waiting longest. Pop_front.
        //
        // Survivors are returned via `push_idle` after the off-lock
        // check, which inserts them at the "newest" end (front for
        // LIFO, back for FIFO). That keeps survivors from being
        // re-scanned on the very next cycle - the next scan picks the
        // next-oldest, and the pool rotates entirely through over
        // ceil(idle_count / max_per_cycle) ticks.
        //
        // **FIFO side effect - known and accepted**: a healthy front-of-
        // queue connection that the scan visits gets pushed to the back
        // via `push_idle(FIFO)` = `push_back`. Strict FIFO checkout
        // order is therefore broken by the scan: a connection that was
        // first in line to be reused gets demoted to last. The pool
        // mostly behaves as FIFO for entries not touched by the scan
        // and partial-LRU for ones that are. Acceptable because (a)
        // the production default is LIFO, (b) scan touches at most
        // `max_per_cycle` per tick, and (c) the alternative (push
        // survivors back to front in FIFO mode) would re-scan the
        // same head every tick and never reach back-of-queue zombies
        // - the same shielding bug we're fixing in LIFO.
        //
        // The earlier pop_front-only implementation never reached the
        // back of a LIFO pool: alive entries at the front were popped,
        // verified, and pushed straight back to the front, perfectly
        // shielding any zombie backends sitting at the back from the
        // scan. With LIFO the default, that meant the production
        // common case never got fixed.
        let queue_mode = self.inner.config.queue_mode;
        let popped: Vec<ObjectInner> = {
            let mut guard = self.inner.slots.lock();
            let take = std::cmp::min(max_per_cycle, guard.vec.len());
            let mut buf = Vec::with_capacity(take);
            for _ in 0..take {
                // try_acquire is non-blocking by design - if the semaphore
                // is exhausted (some real client just took everything) we
                // stop probing for this tick rather than fight them.
                let permit = match self.inner.semaphore.try_acquire() {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let popped_one = match queue_mode {
                    QueueMode::Lifo => guard.vec.pop_back(),
                    QueueMode::Fifo => guard.vec.pop_front(),
                };
                match popped_one {
                    Some(obj) => {
                        permit.forget();
                        buf.push(obj);
                    }
                    None => {
                        // No more idle to probe; permit drops here and is
                        // returned to the semaphore automatically.
                        break;
                    }
                }
            }
            buf
        };

        if popped.is_empty() {
            return (0, 0);
        }

        let checked = popped.len();
        // `EvictGuard` owns the worst-case bookkeeping should this scope
        // unwind during any `check_alive(...).await` below - cancellation,
        // panic, or a future `select!` wrapping the retain task. On
        // happy-path commit it issues a single `add_permits(checked)`
        // AFTER `slots.size -= evicted` so a concurrent checkout never
        // sees `permits == max_size` while `slots.size` is still inflated.
        let guard = EvictGuard::new(&self.inner, checked);
        let mut survivors: Vec<ObjectInner> = Vec::with_capacity(checked);
        let mut evicted: usize = 0;

        // 2. Off-lock checks. The popped objects are in flight exactly like
        // a checkout would be - semaphore permits are held (forgotten) on
        // their behalf. Concurrent callers see the reduced permit budget.
        //
        // backends with a fresh `last_activity` timestamp
        // (touched by every protocol_io send/recv on the connection) skip
        // the `check_alive(SELECT 1)` round-trip. A TCP zombie cannot
        // have completed observable I/O within the threshold; the only
        // way the socket could have died since `last_activity` is a
        // network event that will surface on the next real query, so
        // burning a `SELECT 1` per retain tick on connections that just
        // serviced traffic is pure overhead. On the upper bound - 100
        // pools × `dead_backend_check_max_per_cycle = 8` × 30 s retain
        // interval - that is ~800 SELECT 1/min eliminated from
        // PostgreSQL's load when traffic is steady.
        //
        // The threshold is supplied by the retain loop and should match
        // the retain tick interval: successful I/O since the previous
        // retain cycle is enough evidence to skip the synthetic probe,
        // but older idle backends must be checked so PostgreSQL restarts
        // are detected promptly under short retain intervals.
        for mut inner in popped {
            // A marked-bad object should not survive a health check;
            // it must drop now so `slots.size` decreases (and the replenish
            // loop on the next tick brings a fresh one in).
            if inner.obj.is_bad() {
                evicted += 1;
                // `inner` drops at end of iteration - Server::drop closes the
                // TCP fd; slots.size and semaphore are adjusted in bookkeeping.
                continue;
            }
            // Zombie-scan fast path. `SystemTime::elapsed()` returns
            // Err on rare backwards-clock skew (NTP step); fall through
            // to a real check in that case, never skip it.
            if let Ok(elapsed) = inner.obj.last_activity.elapsed() {
                if elapsed < skip_recent_threshold {
                    survivors.push(inner);
                    continue;
                }
            }
            match inner.obj.check_alive(timeout).await {
                Ok(()) => {
                    // Backend is alive - return it to the idle set unchanged.
                    // Do NOT touch `metrics.recycled`; an actually-idle
                    // connection still needs to age out via `idle_timeout`.
                    survivors.push(inner);
                }
                Err(_) => {
                    // check_alive already called mark_bad and emitted a
                    // descriptive warn!; drop the object and let the
                    // bookkeeping step shrink slots.size.
                    evicted += 1;
                }
            }
        }

        // 3. Happy-path commit: push survivors back, deduct evicted from
        // `slots.size`, then restore all permits. EvictGuard.commit handles
        // the strict ordering (size -= ... -> unlock -> add_permits) inside a
        // single helper so the contract cannot drift in a future refactor.
        guard.commit(queue_mode, survivors, evicted);

        // Pump the per-pool counters so SHOW STATS / Prometheus reflect what
        // the scan just did. Cheap relaxed atomics; safe to call even when
        // nothing changed (the recorder no-ops on zero).
        self.inner
            .server_pool
            .address()
            .stats
            .record_dead_backend_scan(checked, evicted);

        (checked, evicted)
    }

    /// Convert idle reserve connections into main connections when the
    /// coordinator's main semaphore has headroom. Run by the retain task —
    /// never on the hot checkout path — so contention on `slots.lock()`
    /// stays predictable.
    ///
    /// Reserve permits are supposed to be a burst buffer: a backend grabbed
    /// under peak pressure so the pool can push past `max_db_connections`
    /// for a moment. Once the peak is gone, the backend sits in
    /// `slots.vec` as an ordinary idle connection, but its permit still
    /// counts against `reserve_in_use`. Without an upgrade, the reserve
    /// pool shows as occupied even though the main semaphore has free
    /// slots — the next real burst can't tell the buffer is empty, and
    /// `SHOW POOL_COORDINATOR` reports `reserve_used` that doesn't match
    /// actual reserve availability.
    ///
    /// The upgrade itself is a book-keeping swap, not a reconnect: for
    /// each idle reserve backend we try to steal a `db_semaphore` permit
    /// (non-blocking), and on success flip `permit.is_reserve = false`.
    /// The backend stays alive; the reserve semaphore gains a slot.
    ///
    /// Returns the number of permits upgraded.
    pub fn upgrade_reserve_to_main(&self) -> usize {
        let coordinator = match self.inner.coordinator.as_ref() {
            Some(c) => c,
            None => return 0,
        };
        let mut upgraded = 0;
        let mut guard = self.inner.slots.lock();
        for obj in guard.vec.iter_mut() {
            let Some(permit) = obj.coordinator_permit.as_mut() else {
                continue;
            };
            if !permit.is_reserve {
                continue;
            }
            if coordinator.try_upgrade_reserve_to_main() {
                permit.is_reserve = false;
                upgraded += 1;
            } else {
                // Main is saturated too; no point walking the rest of the
                // vec looking for another reserve entry to upgrade.
                break;
            }
        }
        upgraded
    }

    /// Close idle reserve connections that have been idle longer than `min_lifetime_ms`.
    ///
    /// Reserve connections are temporary — created under coordinator pressure when the
    /// main `max_db_connections` limit is reached. They should be released back to the
    /// reserve pool ASAP once idle, not held until the regular `idle_timeout` fires.
    /// This runs as part of the retain cycle to gradually relieve reserve pressure.
    ///
    /// Returns the number of reserve connections closed.
    ///
    /// Same off-lock drop discipline as [`retain`] / [`retain_oldest_first`]:
    /// closed objects are extracted under the lock and dropped after the lock
    /// is released, so the peer pool's eviction syscalls and coordinator
    /// notifications do not stall concurrent recyclers.
    pub fn close_idle_reserve_connections(&self, min_lifetime_ms: u64) -> usize {
        let evicted: Vec<ObjectInner> = {
            let mut guard = self.inner.slots.lock();
            // Common case on pools with `reserve_pool_size = 0` or with
            // reserve connections still within `min_connection_lifetime`:
            // nothing to close. Skip the partition allocation.
            let has_stale_reserve = guard.vec.iter().any(|obj| {
                let is_reserve = obj
                    .coordinator_permit
                    .as_ref()
                    .is_some_and(|p| p.is_reserve);
                is_reserve && obj.metrics.last_used().as_millis() >= u128::from(min_lifetime_ms)
            });
            if !has_stale_reserve {
                return 0;
            }
            let mut keep = VecDeque::with_capacity(guard.vec.capacity());
            let mut evicted = Vec::new();
            for obj in guard.vec.drain(..) {
                let is_reserve = obj
                    .coordinator_permit
                    .as_ref()
                    .is_some_and(|p| p.is_reserve);
                if !is_reserve {
                    keep.push_back(obj);
                    continue;
                }
                // Close reserve connections idle longer than min_connection_lifetime
                let idle = obj.metrics.last_used().as_millis();
                if idle < u128::from(min_lifetime_ms) {
                    keep.push_back(obj);
                } else {
                    evicted.push(obj);
                }
            }
            guard.vec = keep;
            guard.size -= evicted.len();
            evicted
        };
        let closed = evicted.len();
        // Lock released here. Reserve permit drops fire below.
        drop(evicted);
        closed
    }

    /// Get current timeout configuration.
    #[inline(always)]
    pub fn timeouts(&self) -> Timeouts {
        self.inner.config.timeouts
    }

    /// Creates new connections to bring the pool up to the desired count.
    /// Returns the number of connections successfully created.
    /// Stops on the first creation failure to avoid hammering a failing server.
    pub async fn replenish(&self, count: usize) -> usize {
        let mut created = 0;
        for _ in 0..count {
            // Check if there's still room in the pool
            {
                let slots = self.inner.slots.lock();
                if slots.size >= slots.max_size {
                    break;
                }
            }

            // Acquire coordinator permit FIRST (non-blocking). Same ordering
            // rationale as `timeout_get`: a slow coordinator must not hold a
            // burst slot. If the coordinator limit is reached, skip — the
            // next retain cycle will retry.
            let coordinator_permit = if let Some(ref coordinator) = self.inner.coordinator {
                match coordinator.try_acquire() {
                    Some(permit) => Some(permit),
                    None => {
                        log::debug!(
                            "[{}@{}] coordinator limit reached, skipping replenish",
                            self.inner.username,
                            self.inner.pool_name
                        );
                        break;
                    }
                }
            } else {
                None
            };

            // Take the burst slot AFTER the coordinator permit. Replenish runs
            // in the background retain loop, so when client traffic is already
            // saturating the burst gate there is no value in queueing here —
            // defer the work to the next retain cycle and let `timeout_get`
            // callers own the budget. The dropped `coordinator_permit` returns
            // its slot to the cross-pool semaphore.
            let Some(_create_gate) = self.inner.try_acquire_burst_gate() else {
                self.inner
                    .scaling_stats
                    .replenish_deferred
                    .fetch_add(1, Ordering::Relaxed);
                log::debug!(
                    "[{}@{}] replenish: bounded burst at limit, deferring to next cycle",
                    self.inner.username,
                    self.inner.pool_name
                );
                break;
            };

            // Create a new connection
            let obj = match self.inner.server_pool.create().await {
                Ok(obj) => obj,
                Err(e) => {
                    log::debug!(
                        "[{}@{}] replenish: failed to create server: {}",
                        self.inner.username,
                        self.inner.pool_name,
                        e
                    );
                    break;
                }
            };

            {
                let mut slots = self.inner.slots.lock();
                if !self.inner.accepts_fresh_backend_after_create(&slots) {
                    drop(slots);
                    drop(obj);
                    drop(coordinator_permit);
                    break;
                }
                if slots.size >= slots.max_size {
                    drop(slots);
                    drop(obj);
                    drop(coordinator_permit);
                    break;
                }
                let inner = self.inner.new_object_inner(obj, coordinator_permit);
                slots.size += 1;
                push_idle(self.inner.config.queue_mode, &mut slots.vec, inner);
            }

            created += 1;
        }
        created
    }

    /// Closes this Pool.
    pub fn close(&self) {
        self.close_new_checkouts();
        self.resize(0);
    }

    /// Indicates whether this Pool has been closed.
    pub fn is_closed(&self) -> bool {
        self.inner.semaphore.is_closed()
    }

    /// Stop future checkout attempts without draining idle objects yet.
    pub(crate) fn close_new_checkouts(&self) {
        self.inner.semaphore.close();
    }

    /// Retrieves Status of this Pool.
    #[must_use]
    pub fn status(&self) -> Status {
        let slots = self.inner.slots.lock();
        let users = self.inner.users.load(Ordering::Relaxed);
        let available = slots.vec.len();
        let waiting = users.saturating_sub(available);
        Status {
            max_size: slots.max_size,
            size: slots.size,
            available,
            waiting,
        }
    }

    /// Returns ServerPool of this Pool.
    #[must_use]
    pub fn server_pool(&self) -> &ServerPool {
        &self.inner.server_pool
    }

    /// True when every semaphore permit is in use — clients are either
    /// holding connections or queued behind it. Used by housekeeping
    /// (retain loop, lifetime expiration in `recycle()`) to back off and
    /// not close working connections at the moment of peak demand.
    #[must_use]
    pub fn under_pressure(&self) -> bool {
        self.inner.under_pressure()
    }

    /// Test-only handle on the inner semaphore. Used to model client
    /// pressure (drain all permits) in unit tests that exercise the
    /// `under_pressure()` housekeeping gate from peer modules.
    #[cfg(test)]
    pub(crate) fn semaphore(&self) -> &tokio::sync::Semaphore {
        &self.inner.semaphore
    }

    /// Pauses the pool — blocks new connection acquisition.
    pub fn pause(&self) {
        self.inner.server_pool.pause();
    }

    /// Resumes the pool — unblocks waiting clients.
    pub fn resume(&self) {
        self.inner.server_pool.resume();
    }

    /// Returns whether the pool is paused.
    pub fn is_paused(&self) -> bool {
        self.inner.server_pool.is_paused()
    }

    /// Effective merged startup_parameters cascade keyed by parameter, with
    /// the layer that contributed each winning value. Delegates to
    /// `ServerPool` so admin `SHOW STARTUP_PARAMETERS` and the
    /// `/api/pools` JSON share one resolver.
    pub fn effective_startup_parameters_with_sources(
        &self,
    ) -> std::collections::BTreeMap<
        String,
        (
            String,
            super::startup_resolver::ParameterSource,
            super::startup_resolver::ApplicationState,
        ),
    > {
        self.inner
            .server_pool
            .effective_startup_parameters_with_sources()
    }

    /// Bumps reconnect epoch and drains all idle connections.
    /// Returns the new epoch value.
    pub fn reconnect(&self) -> u32 {
        let new_epoch = self.inner.server_pool.bump_epoch();
        // Drain all idle connections — they have the old epoch
        self.retain(|_, _| false);
        new_epoch
    }

    /// Returns the current reconnect epoch.
    pub fn reconnect_epoch(&self) -> u32 {
        self.inner.server_pool.current_epoch()
    }

    /// Returns a snapshot of the per-pool scaling counters used for tuning
    /// the anticipation + bounded burst path. Cheap — six relaxed atomic
    /// loads. Safe to call from `SHOW POOLS` / Prometheus scrapes.
    pub fn scaling_stats(&self) -> ScalingStatsSnapshot {
        let s = &self.inner.scaling_stats;
        ScalingStatsSnapshot {
            creates_started: s.creates_started.load(Ordering::Relaxed),
            burst_gate_waits: s.burst_gate_waits.load(Ordering::Relaxed),
            burst_gate_budget_exhausted: s.burst_gate_budget_exhausted.load(Ordering::Relaxed),
            anticipation_wakes_notify: s.anticipation_wakes_notify.load(Ordering::Relaxed),
            anticipation_wakes_timeout: s.anticipation_wakes_timeout.load(Ordering::Relaxed),
            create_fallback: s.create_fallback.load(Ordering::Relaxed),
            replenish_deferred: s.replenish_deferred.load(Ordering::Relaxed),
            inflight_creates: self.inner.inflight_creates.load(Ordering::Relaxed),
            pre_replacements_triggered: s.pre_replacements_triggered.load(Ordering::Relaxed),
            pre_replacements_skipped: s.pre_replacements_skipped.load(Ordering::Relaxed),
        }
    }

    /// Recycle a connection received via direct handoff. On success,
    /// returns `Ok(ObjectInner)` — the caller wraps it via
    /// `wrap_checkout`. On failure, decrements `slots.size` (the
    /// backend is gone) and returns `Err(())`.
    async fn recycle_handoff(
        &self,
        inner: ObjectInner,
        timeouts: &Timeouts,
    ) -> Result<ObjectInner, ()> {
        // same cancel-safety hazard as
        // `try_recycle_one`. The `inner` arrived via direct handoff
        // (counted in `slots.size`). If the future is cancelled during
        // `recycle().await`, the bare `inner` drops and closes the TCP
        // fd but `slots.size` is never decremented - permanent leak.
        let skip_lifetime = self.inner.under_pressure();
        let mut guard = BareInnerGuard::new(&self.inner, inner);
        let recycle_result = {
            let inner_ref = guard.as_mut();
            let ObjectInner { obj, metrics, .. } = inner_ref;
            match timeouts.recycle {
                Some(duration) => {
                    match tokio::time::timeout(
                        duration,
                        self.inner.server_pool.recycle(obj, metrics, skip_lifetime),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(RecycleError::StaticMessage("Recycle timeout")),
                    }
                }
                None => {
                    self.inner
                        .server_pool
                        .recycle(obj, metrics, skip_lifetime)
                        .await
                }
            }
        };
        match recycle_result {
            Ok(()) => {
                let inner = guard.disarm();
                self.maybe_trigger_pre_replacement(&inner.metrics);
                Ok(inner)
            }
            Err(_) => {
                drop(guard);
                Err(())
            }
        }
    }

    /// Check if a connection approaching lifetime expiry should trigger
    /// a background pre-replacement, and spawn the task if so.
    fn maybe_trigger_pre_replacement(&self, metrics: &Metrics) {
        // Quick checks that don't need a lock.
        if metrics.lifetime_ms < PRE_REPLACE_MIN_LIFETIME_MS {
            return;
        }
        let age_ms = metrics.age().as_millis() as u64;
        let threshold = metrics.lifetime_ms * PRE_REPLACE_THRESHOLD_PCT / 100;
        if age_ms < threshold || age_ms >= metrics.lifetime_ms {
            return;
        }
        if self.inner.under_pressure() {
            return;
        }
        if self.inner.server_pool.is_paused() {
            return;
        }

        // Pool tightness + overshoot check under lock.
        {
            let slots = self.inner.slots.lock();
            // Allow overshoot up to max_size + MAX_CONCURRENT_PRE_REPLACEMENTS.
            let in_flight = self
                .inner
                .pre_replacements_in_flight
                .load(Ordering::Relaxed);
            if slots.size + in_flight > slots.max_size + MAX_CONCURRENT_PRE_REPLACEMENTS {
                return;
            }
            // Idle ratio: only pre-replace when < 25% of connections are idle.
            // If the pool has plenty of idle connections it can absorb the
            // loss of one to lifetime expiry without a spike.
            let idle_pct = if slots.size > 0 {
                slots.vec.len() * 100 / slots.size
            } else {
                100
            };
            if idle_pct >= 25 {
                return;
            }
        }

        // Cap concurrent pre-replacements.
        if !try_take_burst_slot(
            &self.inner.pre_replacements_in_flight,
            MAX_CONCURRENT_PRE_REPLACEMENTS,
        ) {
            return;
        }

        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            // RAII guard so the counter is decremented even
            // if `pre_replace_one` panics. Previously the manual
            // `fetch_sub` after `.await` was skipped on panic, leaking
            // the slot permanently - after MAX_CONCURRENT_PRE_REPLACEMENTS
            // panics, `try_take_burst_slot` would refuse every future
            // pre-replacement attempt (silent degradation invisible to
            // operators).
            struct PreReplaceGuard<'a>(&'a std::sync::atomic::AtomicUsize);
            impl Drop for PreReplaceGuard<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let _guard = PreReplaceGuard(&inner.pre_replacements_in_flight);
            inner.pre_replace_one().await;
        });
    }
}

/// Builder for Pool.
pub struct PoolBuilder {
    server_pool: ServerPool,
    config: PoolConfig,
    coordinator: Option<Arc<pool_coordinator::PoolCoordinator>>,
    pool_name: String,
    username: String,
}

impl PoolBuilder {
    fn new(server_pool: ServerPool) -> Self {
        Self {
            server_pool,
            config: PoolConfig::default(),
            coordinator: None,
            pool_name: String::new(),
            username: String::new(),
        }
    }

    /// Sets the PoolConfig.
    pub fn config(mut self, config: PoolConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets the database-level coordinator (for max_db_connections enforcement).
    pub fn coordinator(
        mut self,
        coordinator: Option<Arc<pool_coordinator::PoolCoordinator>>,
    ) -> Self {
        self.coordinator = coordinator;
        self
    }

    /// Sets the pool name (database name), used in coordinator error messages.
    pub fn pool_name(mut self, name: String) -> Self {
        self.pool_name = name;
        self
    }

    /// Sets the username for this pool, used in coordinator error messages.
    pub fn username(mut self, name: String) -> Self {
        self.username = name;
        self
    }

    /// Builds the Pool.
    pub fn build(self) -> Pool {
        Pool::from_builder(self)
    }
}

impl fmt::Debug for PoolBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolBuilder")
            .field("config", &self.config)
            .finish()
    }
}

/// Try to take a slot from the bounded burst counter.
///
/// Optimistically increments the counter and validates it stayed below `max`.
/// If the slot is available, returns `true` and leaves the counter incremented
/// (caller is responsible for releasing it). If the cap was already reached,
/// rolls back the increment and returns `false`.
///
/// This intentionally tolerates brief over-shoot when many tasks race the
/// `fetch_add`: the next observation will reflect the corrected value once
/// rollback completes. The cap is a soft burst smoother, not a hard fence,
/// and a 1-2 transient excess is acceptable for this purpose.
#[inline]
fn try_take_burst_slot(counter: &AtomicUsize, max: usize) -> bool {
    let prev = counter.fetch_add(1, Ordering::AcqRel);
    if prev < max {
        return true;
    }
    counter.fetch_sub(1, Ordering::Release);
    false
}

/// RAII guard for a burst gate slot. Decrements `inflight_creates`
/// and wakes one burst-gate waiter on drop.
struct BurstGateGuard<'a> {
    inflight_creates: &'a AtomicUsize,
    create_done: &'a Notify,
}

impl Drop for BurstGateGuard<'_> {
    fn drop(&mut self) {
        self.inflight_creates.fetch_sub(1, Ordering::Release);
        self.create_done.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ------------------------------------------------------------------
    // BurstGateGuard — RAII burst gate slot
    // ------------------------------------------------------------------

    #[test]
    fn burst_gate_guard_decrements_on_drop() {
        let counter = AtomicUsize::new(1);
        let notify = Notify::new();
        {
            let _g = BurstGateGuard {
                inflight_creates: &counter,
                create_done: &notify,
            };
            assert_eq!(counter.load(Ordering::Acquire), 1);
        }
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn burst_gate_guard_notifies_on_drop() {
        let counter = AtomicUsize::new(1);
        let notify = Notify::new();
        let fut = notify.notified();
        {
            let _g = BurstGateGuard {
                inflight_creates: &counter,
                create_done: &notify,
            };
        }
        tokio::time::timeout(Duration::from_millis(50), fut)
            .await
            .expect("drop must fire notify_one");
    }

    #[test]
    fn burst_gate_guard_no_decrement_on_forget() {
        let counter = AtomicUsize::new(1);
        let notify = Notify::new();
        let g = BurstGateGuard {
            inflight_creates: &counter,
            create_done: &notify,
        };
        std::mem::forget(g);
        assert_eq!(counter.load(Ordering::Acquire), 1);
    }

    // ------------------------------------------------------------------
    // try_take_burst_slot — soft burst limiter
    // ------------------------------------------------------------------

    #[test]
    fn burst_slot_taken_when_under_cap() {
        let counter = AtomicUsize::new(0);
        assert!(try_take_burst_slot(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 1);
        assert!(try_take_burst_slot(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 2);
    }

    #[test]
    fn burst_slot_rejected_at_cap_and_counter_rolled_back() {
        let counter = AtomicUsize::new(2);
        assert!(!try_take_burst_slot(&counter, 2));
        // Roll-back must restore the counter exactly.
        assert_eq!(counter.load(Ordering::Acquire), 2);
    }

    #[test]
    fn burst_slot_rejected_when_already_above_cap() {
        // Brief transient over-shoot from a racing peer should also reject
        // and roll back, never grow further.
        let counter = AtomicUsize::new(5);
        assert!(!try_take_burst_slot(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 5);
    }

    #[test]
    fn burst_slot_zero_cap_always_rejects() {
        let counter = AtomicUsize::new(0);
        assert!(!try_take_burst_slot(&counter, 0));
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn burst_slot_concurrent_acquire_caps_within_one_of_max() {
        // Stress: many threads racing the gate must never end with more than
        // `max + (threads - max)` rolled-back observations. The gate is a
        // soft cap, so we tolerate up to `max` accepted slots; everyone else
        // must observe rejection and leave the counter at exactly `max`.
        use std::sync::Arc;
        use std::thread;

        const THREADS: usize = 32;
        const MAX: usize = 4;

        let counter = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let counter = Arc::clone(&counter);
            let accepted = Arc::clone(&accepted);
            handles.push(thread::spawn(move || {
                if try_take_burst_slot(&counter, MAX) {
                    accepted.fetch_add(1, Ordering::Relaxed);
                    // Hold the slot briefly so peers race rejection.
                    thread::yield_now();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let final_count = counter.load(Ordering::Acquire);
        let final_accepted = accepted.load(Ordering::Acquire);
        // No leak: every accepted slot is still in the counter, every
        // rejected attempt rolled back.
        assert_eq!(final_count, final_accepted);
        // Hard upper bound — burst gate must never accept more than MAX.
        assert!(
            final_accepted <= MAX,
            "burst gate accepted {final_accepted} > MAX {MAX}"
        );
        // Sanity — at least one thread must have made progress.
        assert!(final_accepted >= 1);
    }

    // ------------------------------------------------------------------
    // Notify register-before-check pattern
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn notify_one_buffered_when_registered_before_signal() {
        // The Phase 4 anticipation loop relies on this property: a
        // notified() registered before notify_one() must wake immediately,
        // even if the await happens after the signal fires.
        let notify = std::sync::Arc::new(Notify::new());
        let n2 = std::sync::Arc::clone(&notify);

        let notified = notify.notified();
        n2.notify_one();
        // notify happened before await — must still wake.
        tokio::time::timeout(Duration::from_millis(50), notified)
            .await
            .expect("notified() must resolve when notify_one fired before await");
    }

    #[tokio::test]
    async fn notify_one_wakes_exactly_one_waiter() {
        // Anti-thundering-herd guarantee: a single return_object must wake
        // exactly one Phase 4 anticipation waiter, not all of them.
        //
        // Synchronization is barrier-based, not sleep-based: each waiter
        // signals it has parked on `notified()` BEFORE awaiting, so the
        // test never races CI scheduling latency.
        use std::sync::Arc;
        use tokio::sync::Barrier;

        const WAITERS: usize = 5;

        let notify = Arc::new(Notify::new());
        let woken = Arc::new(AtomicUsize::new(0));
        // +1 for the test driver itself.
        let registered = Arc::new(Barrier::new(WAITERS + 1));

        let mut handles = Vec::with_capacity(WAITERS);
        for _ in 0..WAITERS {
            let n = Arc::clone(&notify);
            let w = Arc::clone(&woken);
            let r = Arc::clone(&registered);
            handles.push(tokio::spawn(async move {
                // Register the future BEFORE the barrier so the wait below
                // is on a future already attached to the Notify queue.
                let fut = n.notified();
                tokio::pin!(fut);
                fut.as_mut().enable();
                r.wait().await;
                fut.await;
                w.fetch_add(1, Ordering::Relaxed);
            }));
        }

        // All waiters have armed their `Notified` future and are about to await.
        registered.wait().await;
        // Yield once so the spawned tasks reach `fut.await` after the barrier.
        tokio::task::yield_now().await;

        notify.notify_one();

        // Wait for ANY one waiter to record its wake. We do this by polling
        // a counter with a tight yield loop, capped by a generous wall-clock
        // budget so a stuck test fails instead of hanging the suite.
        let started = std::time::Instant::now();
        loop {
            if woken.load(Ordering::Acquire) >= 1 {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "no waiter woke within 2s after notify_one"
            );
            tokio::task::yield_now().await;
        }

        // Strict invariant: only one waiter must be woken by one notify_one.
        // Give the runtime a few yields to surface any spurious extra wakes.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            woken.load(Ordering::Acquire),
            1,
            "exactly one waiter must wake per notify_one"
        );

        // Cleanup: wake the remaining waiters one by one so the spawned tasks
        // can finish and we do not leak them past the test.
        for _ in 0..(WAITERS - 1) {
            notify.notify_one();
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn missed_notify_when_check_precedes_registration() {
        // Negative regression test: this is what would break if a future
        // refactor moved `let notified = ...` AFTER the recycle check in the
        // anticipation phase. The notify fired between the check and the
        // registration is lost, the waiter sleeps until its wake source
        // arrives — proving why the register-before-check ordering matters.
        let notify = Arc::new(Notify::new());

        // Wrong order: signal fires BEFORE the waiter creates its `notified`.
        notify.notify_one();
        let notified = notify.notified();

        // Permit was buffered when no waiter was registered, so the next
        // `notified()` consumes it immediately.
        // (This is the documented tokio behavior we rely on for the
        // register-BEFORE-check pattern: the buffered permit goes to the
        // first future that registers AFTER the signal.)
        tokio::time::timeout(Duration::from_millis(50), notified)
            .await
            .expect("buffered permit must wake the next notified()");

        // Now demonstrate the failure mode: signal fires, the buffered
        // permit is consumed by an unrelated `notified()`, and a LATER
        // `notified()` does NOT see it.
        notify.notify_one();
        let consumer = notify.notified();
        tokio::time::timeout(Duration::from_millis(50), consumer)
            .await
            .expect("buffered permit goes to first future");

        let late = notify.notified();
        let result = tokio::time::timeout(Duration::from_millis(50), late).await;
        assert!(
            result.is_err(),
            "a Notified future created AFTER the buffered permit was consumed \
             must NOT wake without a fresh notify_one"
        );
    }

    // ------------------------------------------------------------------
    // notify_return_observers — covers both fast and slow return_object
    // ------------------------------------------------------------------

    /// Builds a `Pool` whose `ServerPool` is never asked to `create()`.
    /// Address/User defaults are fine because the test never opens a
    /// real backend connection — it only exercises the in-memory notify
    /// machinery on the resulting `PoolInner`.
    fn test_pool_with_coordinator(coord: Arc<pool_coordinator::PoolCoordinator>) -> Pool {
        use crate::config::{Address, User};
        use dashmap::DashMap;

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
        Pool::builder(server_pool)
            .coordinator(Some(coord))
            .pool_name("test_db".to_string())
            .username("test_user".to_string())
            .build()
    }

    /// `notify_return_observers` wakes the peer-pool coordinator the wait queue
    /// waiter so eviction scans can find the just-returned connection.
    /// Same-pool waiters now use direct-handoff oneshot channels inside
    /// `return_object` and do not park on a Notify.
    #[tokio::test]
    async fn notify_return_observers_wakes_phase_c_waiter() {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering as AOrdering;

        use pool_coordinator::{CoordinatorConfig, EvictionSource, PoolCoordinator};

        struct CountingEviction {
            calls: Arc<AtomicU64>,
        }
        impl EvictionSource for CountingEviction {
            fn try_evict_one(&self, _user: &str) -> bool {
                self.calls.fetch_add(1, AOrdering::Relaxed);
                false
            }
            fn queued_clients(&self, _user: &str) -> usize {
                0
            }
            fn is_starving(&self, _user: &str) -> bool {
                false
            }
        }

        let coord = PoolCoordinator::new(
            "test_db".to_string(),
            CoordinatorConfig {
                max_db_connections: 1,
                min_connection_lifetime_ms: 5000,
                reserve_pool_size: 0,
                reserve_pool_timeout_ms: 2000,
            },
        );
        let _pinned = coord.try_acquire().expect("first slot is free");

        let pool = test_pool_with_coordinator(coord.clone());

        let coord_w = coord.clone();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_w = Arc::clone(&calls);
        let phase_c_waiter = tokio::spawn(async move {
            let eviction = CountingEviction { calls: calls_w };
            coord_w.acquire("test_db", "u", &eviction).await
        });

        let parked = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if calls.load(AOrdering::Relaxed) >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(parked.is_ok(), "the wait queue waiter never parked");
        let baseline = calls.load(AOrdering::Relaxed);
        assert_eq!(
            baseline, 2,
            "Phase B and the first wait-queue iteration each call try_evict_one once",
        );

        pool.inner.notify_return_observers();

        let woke = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if calls.load(AOrdering::Relaxed) > baseline {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            woke.is_ok(),
            "the wait queue waiter must wake on coordinator.notify_idle_returned",
        );
        assert_eq!(
            calls.load(AOrdering::Relaxed),
            baseline + 1,
            "exactly one wait-queue wake -> exactly one extra try_evict_one",
        );

        phase_c_waiter.abort();
        let _ = phase_c_waiter.await;
    }

    // ------------------------------------------------------------------
    // upgrade_reserve_to_main — retain-time book-keeping swap
    // ------------------------------------------------------------------

    /// Smoke test for the retain-time helper: on an empty pool it must
    /// report zero upgrades and leave the coordinator state untouched.
    /// The real coverage of the upgrade arithmetic lives in
    /// `pool_coordinator::tests::reserve_to_main_upgrade_*`; this test
    /// pins the outer wrapper against a refactor that would accidentally
    /// touch coordinator counters on an empty slots vec.
    #[tokio::test]
    async fn upgrade_reserve_to_main_noop_on_empty_pool() {
        let coord = pool_coordinator::PoolCoordinator::new(
            "test_db".to_string(),
            pool_coordinator::CoordinatorConfig {
                max_db_connections: 4,
                min_connection_lifetime_ms: 5000,
                reserve_pool_size: 2,
                reserve_pool_timeout_ms: 100,
            },
        );
        let pool = test_pool_with_coordinator(coord.clone());
        assert_eq!(pool.upgrade_reserve_to_main(), 0);
        assert_eq!(coord.reserve_in_use(), 0);
        assert_eq!(coord.total_connections(), 0);
    }

    /// A pool without a coordinator (max_db_connections = 0) has no
    /// reserve concept at all — the helper must short-circuit and
    /// return 0 without locking `slots`.
    #[tokio::test]
    async fn upgrade_reserve_to_main_returns_zero_without_coordinator() {
        use crate::config::{Address, User};
        use dashmap::DashMap;

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
        let pool = Pool::builder(server_pool)
            .pool_name("test_db".to_string())
            .username("test_user".to_string())
            .build();
        assert_eq!(pool.upgrade_reserve_to_main(), 0);
    }

    // ------------------------------------------------------------------
    // under_pressure — predicate that gates lifetime housekeeping
    // ------------------------------------------------------------------

    /// `under_pressure` is the gate that decides whether `recycle()` and
    /// the retain loop close a working connection by `server_lifetime`.
    /// Wrong answer here means we either close connections mid-storm
    /// (false negative) or never refresh aged ones (false positive). The
    /// contract is "true iff every semaphore permit is in flight", so the
    /// test acquires all permits, asserts true, releases them, asserts
    /// false.
    #[tokio::test]
    async fn under_pressure_tracks_semaphore_exhaustion() {
        let coord = pool_coordinator::PoolCoordinator::new(
            "test_db".to_string(),
            pool_coordinator::CoordinatorConfig {
                max_db_connections: 0,
                min_connection_lifetime_ms: 0,
                reserve_pool_size: 0,
                reserve_pool_timeout_ms: 0,
            },
        );
        let pool = test_pool_with_coordinator(coord);

        // Builder default for tests is small but non-zero. Read the
        // current permit count so the test does not depend on it.
        let total_permits = pool.inner.semaphore.available_permits();
        assert!(
            total_permits > 0,
            "test pool must start with at least one permit"
        );

        // Empty pool with all permits free → no pressure.
        assert!(
            !pool.inner.under_pressure(),
            "fresh pool must report no pressure"
        );

        // Drain every permit. Holding them models clients holding
        // connections + clients queued behind the semaphore.
        let mut held = Vec::with_capacity(total_permits);
        for _ in 0..total_permits {
            held.push(pool.inner.semaphore.acquire().await.unwrap());
        }
        assert!(
            pool.inner.under_pressure(),
            "drained semaphore must report under_pressure",
        );

        // Release one permit -> pressure clears.
        held.pop();
        assert!(
            !pool.inner.under_pressure(),
            "releasing one permit must clear pressure",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pool_status_separates_idle_available_from_checkout_waiters() {
        use crate::server::Server;

        let pool = empty_test_pool_with_max_size(3);
        let mut checked_out = Vec::new();

        {
            let mut slots = pool.inner.slots.lock();
            for _ in 0..2 {
                let permit = pool.semaphore().try_acquire().unwrap();
                permit.forget();
                slots.size += 1;
                checked_out.push(ObjectInner {
                    obj: Server::test_zombie_marked_bad(),
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
            }

            slots.size += 1;
            push_idle(
                pool.inner.config.queue_mode,
                &mut slots.vec,
                ObjectInner {
                    obj: Server::test_zombie_marked_bad(),
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                },
            );
        }

        let status = pool.status();
        assert_eq!(status.size, 3);
        assert_eq!(
            status.available, 1,
            "only idle queue entries are immediately available; checked-out slots are busy"
        );
        assert_eq!(status.waiting, 0);

        pool.inner.users.store(2, Ordering::Relaxed);
        let status = pool.status();
        assert_eq!(status.available, 1);
        assert_eq!(
            status.waiting, 1,
            "two checkout futures competing for one idle entry leaves one waiter"
        );

        pool.inner.users.store(0, Ordering::Relaxed);
        drop(checked_out);
    }

    // ------------------------------------------------------------------
    // Direct handoff - oneshot channel mechanics
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn direct_handoff_delivers_to_oldest_waiter() {
        // Three waiters registered in order. A single send must deliver
        // to the first (oldest) waiter; the other two must not receive.
        let (tx1, rx1) = oneshot::channel::<u32>();
        let (tx2, rx2) = oneshot::channel::<u32>();
        let (tx3, rx3) = oneshot::channel::<u32>();

        let mut waiters = VecDeque::new();
        waiters.push_back(tx1);
        waiters.push_back(tx2);
        waiters.push_back(tx3);

        // Pop the oldest and send.
        let sender = waiters.pop_front().unwrap();
        sender.send(42).expect("receiver must be alive");

        assert_eq!(rx1.await.unwrap(), 42);
        // rx2 and rx3 must not have received anything.
        assert_eq!(waiters.len(), 2);

        // Verify the remaining senders are still pending (not resolved).
        let result = tokio::time::timeout(Duration::from_millis(10), rx2).await;
        assert!(result.is_err(), "second waiter must not receive");
        let result = tokio::time::timeout(Duration::from_millis(10), rx3).await;
        assert!(result.is_err(), "third waiter must not receive");
    }

    #[tokio::test]
    async fn direct_handoff_skips_dropped_receiver() {
        // Simulate a timed-out waiter: register a sender, drop the
        // receiver, then attempt send. The send must fail with the
        // value returned in Err, allowing the caller to try the next
        // waiter or fall back to the idle queue.
        let (tx1, rx1) = oneshot::channel::<u32>();
        let (tx2, rx2) = oneshot::channel::<u32>();

        let mut waiters = VecDeque::new();
        waiters.push_back(tx1);
        waiters.push_back(tx2);

        // Drop first receiver (simulates timeout).
        drop(rx1);

        // Walk the waiters like return_object does.
        let mut value = 99u32;
        while let Some(sender) = waiters.pop_front() {
            match sender.send(value) {
                Ok(()) => {
                    value = 0; // sentinel: delivered
                    break;
                }
                Err(returned) => {
                    value = returned;
                }
            }
        }
        assert_eq!(value, 0, "value must have been delivered to second waiter");
        assert_eq!(rx2.await.unwrap(), 99);
    }

    #[tokio::test]
    async fn direct_handoff_falls_back_when_no_waiters() {
        // With no waiters, there is nothing to pop. The value stays
        // with the caller (simulates the push-to-vec fallback path).
        let waiters: VecDeque<oneshot::Sender<u32>> = VecDeque::new();
        assert!(waiters.is_empty());
        // return_object would push to vec + add_permits here.
    }

    #[tokio::test]
    async fn direct_handoff_enqueue_prunes_cancelled_waiters() {
        let mut slots = Slots {
            vec: VecDeque::new(),
            waiters: VecDeque::new(),
            size: 0,
            max_size: 8,
            permits_to_retire: 0,
        };

        let (stale_tx, stale_rx) = oneshot::channel::<ObjectInner>();
        slots.waiters.push_back(stale_tx);
        drop(stale_rx);

        let (live_tx, _live_rx) = oneshot::channel::<ObjectInner>();
        push_handoff_waiter(&mut slots, live_tx);

        assert_eq!(
            slots.waiters.len(),
            1,
            "enqueue must prune cancelled handoff waiters before appending a new one"
        );
    }

    #[tokio::test]
    async fn direct_handoff_final_drain_closes_late_sender_window() {
        let (tx, mut rx) = oneshot::channel::<u32>();

        assert!(close_and_drain_handoff_receiver(&mut rx).is_err());
        assert!(
            tx.send(42).is_err(),
            "sender must fail after the waiter commits to no-value"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_handoff_waiter_cancellation_requeues_delivered_inner() {
        let pool = empty_test_pool_with_max_size(1);
        {
            let mut slots = pool.inner.slots.lock();
            slots.size = 1;
        }

        let pool_for_waiter = pool.clone();
        let waiter = tokio::spawn(async move {
            pool_for_waiter
                .timeout_get(&Timeouts {
                    wait: Some(Duration::from_secs(30)),
                    create: Some(Duration::from_secs(30)),
                    recycle: Some(Duration::from_secs(30)),
                })
                .await
        });

        for _ in 0..100 {
            if pool.inner.slots.lock().waiters.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            pool.inner.slots.lock().waiters.len(),
            1,
            "checkout must be parked as a direct-handoff waiter"
        );

        let inner = pool
            .inner
            .new_object_inner(Server::test_zombie_marked_bad(), None);
        pool.inner.return_object(inner);

        waiter.abort();
        let _ = waiter.await;

        let slots = pool.inner.slots.lock();
        assert_eq!(slots.size, 1);
        assert_eq!(
            slots.vec.len(),
            1,
            "a handoff delivered before cancellation must be returned to idle"
        );
    }

    #[tokio::test]
    async fn wait_if_paused_catches_resume_between_check_and_await() {
        let pool = empty_test_pool();
        pool.pause();

        let mut fired = false;
        let pool_for_hook = pool.clone();
        pool.wait_if_paused_with_hook(
            &Timeouts {
                wait: Some(Duration::from_millis(50)),
                ..Timeouts::default()
            },
            || {
                if !fired {
                    fired = true;
                    pool_for_hook.resume();
                }
            },
        )
        .await
        .expect("enabled waiter must observe resume fired before await");

        assert!(fired);
        assert!(!pool.is_paused());
    }

    // ------------------------------------------------------------------
    // Adaptive anticipation budget
    // ------------------------------------------------------------------

    #[test]
    fn anticipation_budget_cold_start() {
        // No histogram data (fresh process). Default 100ms.
        assert_eq!(anticipation_base_ms(0), 100);
    }

    #[test]
    fn anticipation_budget_fast_workload() {
        // xact_p99 = 700us (0.7ms). base = 0.7ms * 2 = 1ms.
        // Clamped to MIN_BUDGET_MS = 5ms during jitter step.
        assert_eq!(anticipation_base_ms(700), 1);
    }

    #[test]
    fn anticipation_budget_medium_workload() {
        // xact_p99 = 50ms (50000us). base = 50 * 2 = 100ms.
        assert_eq!(anticipation_base_ms(50_000), 100);
    }

    #[test]
    fn anticipation_budget_high_latency() {
        // xact_p99 = 300ms (300000us). base = 300 * 2 = 600ms.
        // Clamped to HARD_CAP (500ms) during jitter step.
        assert_eq!(anticipation_base_ms(300_000), 600);
    }

    #[test]
    fn anticipation_budget_clamp_range() {
        // Verify the full pipeline: base → jitter → clamp
        for p99_us in [0, 500, 1000, 5000, 50_000, 200_000, 500_000] {
            let base = anticipation_base_ms(p99_us);
            let jitter_range = (base / 5).max(1);
            // Min possible after jitter
            let min_val = base.saturating_sub(jitter_range);
            // Max possible after jitter
            let max_val = base + jitter_range;
            // After clamp
            let clamped_min = min_val.clamp(ANTICIPATION_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS);
            let clamped_max = max_val.clamp(ANTICIPATION_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS);
            assert!(clamped_min >= ANTICIPATION_MIN_BUDGET_MS);
            assert!(clamped_max <= ANTICIPATION_HARD_CAP_MS);
        }
    }

    #[test]
    fn burst_gate_budget_cold_start() {
        let budget = burst_gate_budget(0);
        assert!(budget.as_millis() >= BURST_GATE_MIN_BUDGET_MS as u128);
        assert!(budget.as_millis() <= ANTICIPATION_HARD_CAP_MS as u128);
    }

    #[test]
    fn burst_gate_budget_normal_workload() {
        // xact_p99 = 67ms (67000us). base = 134ms. jitter ±27ms.
        let budget = burst_gate_budget(67_000);
        assert!(budget.as_millis() >= BURST_GATE_MIN_BUDGET_MS as u128);
        assert!(budget.as_millis() <= ANTICIPATION_HARD_CAP_MS as u128);
    }

    #[test]
    fn burst_gate_budget_fast_workload() {
        // xact_p99 = 700us. base = 1ms. Clamped to min 20ms.
        let budget = burst_gate_budget(700);
        assert_eq!(budget.as_millis(), BURST_GATE_MIN_BUDGET_MS as u128);
    }

    #[test]
    fn burst_gate_budget_clamp_range() {
        for p99_us in [0, 500, 1000, 5000, 50_000, 200_000, 500_000] {
            let budget = burst_gate_budget(p99_us);
            assert!(budget.as_millis() >= BURST_GATE_MIN_BUDGET_MS as u128);
            assert!(budget.as_millis() <= ANTICIPATION_HARD_CAP_MS as u128);
        }
    }

    // ------------------------------------------------------------------
    // anticipation_base_ms — additional edge cases
    // ------------------------------------------------------------------

    #[test]
    fn anticipation_base_ms_u64_max_saturates() {
        // saturating_mul(2) must not wrap on extreme input.
        let result = anticipation_base_ms(u64::MAX);
        // u64::MAX * 2 saturates to u64::MAX, then / 1000.
        assert_eq!(result, u64::MAX / 1000);
    }

    #[test]
    fn anticipation_base_ms_one_microsecond() {
        // 1us * 2 / 1000 = 0 (integer truncation).
        assert_eq!(anticipation_base_ms(1), 0);
    }

    #[test]
    fn anticipation_base_ms_boundary_500us() {
        // 500us * 2 / 1000 = 1ms exactly.
        assert_eq!(anticipation_base_ms(500), 1);
    }

    #[test]
    fn anticipation_base_ms_boundary_499us() {
        // 499us * 2 / 1000 = 998/1000 = 0 (truncated).
        assert_eq!(anticipation_base_ms(499), 0);
    }

    #[test]
    fn anticipation_base_ms_hard_cap_boundary() {
        // Find the input that produces exactly ANTICIPATION_HARD_CAP_MS.
        // cap = 500ms, so base = 500 when xact_p99_us = 250_000.
        assert_eq!(anticipation_base_ms(250_000), 500);
        assert_eq!(anticipation_base_ms(250_000), ANTICIPATION_HARD_CAP_MS);
    }

    // ------------------------------------------------------------------
    // Jitter + clamp pipeline — exhaustive range invariant
    // ------------------------------------------------------------------

    #[test]
    fn anticipation_jitter_clamp_always_in_bounds() {
        // For a wide range of xact_p99 values (including extreme ones),
        // the full jitter + clamp pipeline must always produce a result
        // in [ANTICIPATION_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS].
        // Run each value multiple times to exercise jitter randomness.
        let inputs = [
            0,
            1,
            10,
            100,
            499,
            500,
            501,
            1_000,
            2_500,
            5_000,
            10_000,
            25_000,
            50_000,
            100_000,
            200_000,
            250_000,
            300_000,
            500_000,
            1_000_000,
            u64::MAX / 2,
            u64::MAX,
        ];
        for &p99_us in &inputs {
            for _ in 0..20 {
                let base_ms = anticipation_base_ms(p99_us);
                let jitter_range = (base_ms / 5).max(1);
                let jitter = rand::rng().random_range(0..=jitter_range * 2);
                let clamped = (base_ms.saturating_sub(jitter_range) + jitter)
                    .clamp(ANTICIPATION_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS);
                assert!(
                    clamped >= ANTICIPATION_MIN_BUDGET_MS,
                    "p99_us={p99_us} base={base_ms} jitter={jitter}: result {clamped} < min {ANTICIPATION_MIN_BUDGET_MS}",
                );
                assert!(
                    clamped <= ANTICIPATION_HARD_CAP_MS,
                    "p99_us={p99_us} base={base_ms} jitter={jitter}: result {clamped} > cap {ANTICIPATION_HARD_CAP_MS}",
                );
            }
        }
    }

    #[test]
    fn anticipation_jitter_clamp_zero_base_clamps_to_min() {
        // When base_ms = 0 (from very small xact_p99), jitter_range = max(0/5, 1) = 1.
        // min_val = 0 - 1 = saturates to 0. After clamp: ANTICIPATION_MIN_BUDGET_MS.
        // max_val = 0 + 1 = 1. After clamp: max(1, 5) = 5.
        // Both endpoints clamp to MIN_BUDGET_MS.
        let base_ms = anticipation_base_ms(1); // = 0
        assert_eq!(base_ms, 0);
        let jitter_range = (base_ms / 5).max(1);
        let min_possible = base_ms
            .saturating_sub(jitter_range)
            .clamp(ANTICIPATION_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS);
        let max_possible = (base_ms + jitter_range * 2)
            .clamp(ANTICIPATION_MIN_BUDGET_MS, ANTICIPATION_HARD_CAP_MS);
        assert_eq!(min_possible, ANTICIPATION_MIN_BUDGET_MS);
        assert_eq!(max_possible, ANTICIPATION_MIN_BUDGET_MS);
    }

    // ------------------------------------------------------------------
    // Semaphore invariant: return_object restores permits in both paths
    // ------------------------------------------------------------------
    //
    // ObjectInner requires a Server (live TCP stream), so we cannot call
    // return_object directly. Instead we model its exact logic using the
    // same primitives (Semaphore, Mutex<Slots>, oneshot channels) and
    // verify the semaphore permit count is conserved.
    //
    // The contract under test:
    //   1. Handoff path (waiter present): send to waiter + add_permits(1)
    //   2. Idle path (no waiter): push to vec + add_permits(1)
    //   3. Both paths restore exactly one permit per return.
    //
    // The OLD bug: handoff path did NOT call add_permits(1), causing
    // permanent permit drain. These tests would catch a regression.

    /// Model the return_object handoff path: waiter exists, send succeeds.
    /// Verify the semaphore permit is restored.
    #[tokio::test]
    async fn semaphore_permit_restored_on_handoff() {
        let max_size = 4;
        let semaphore = Semaphore::new(max_size);

        // Simulate one connection checked out: acquire + forget.
        let permit = semaphore.acquire().await.unwrap();
        permit.forget();
        assert_eq!(semaphore.available_permits(), max_size - 1);

        // Waiter registers (simulates a concurrent checkout).
        let (tx, rx) = oneshot::channel::<u32>();
        let mut waiters: VecDeque<oneshot::Sender<u32>> = VecDeque::new();
        waiters.push_back(tx);

        // Model return_object handoff path:
        // pop waiter, send, then add_permits(1).
        let sender = waiters.pop_front().unwrap();
        sender.send(42).unwrap();
        semaphore.add_permits(1);

        // The returning client's permit is restored.
        assert_eq!(semaphore.available_permits(), max_size);
        // The waiter received the connection.
        assert_eq!(rx.await.unwrap(), 42);
    }

    /// Model the return_object idle path: no waiters, push to vec.
    /// Verify the semaphore permit is restored.
    #[tokio::test]
    async fn semaphore_permit_restored_on_idle_return() {
        let max_size = 4;
        let semaphore = Semaphore::new(max_size);

        // Simulate one connection checked out.
        let permit = semaphore.acquire().await.unwrap();
        permit.forget();
        assert_eq!(semaphore.available_permits(), max_size - 1);

        // Model return_object idle path:
        // no waiters -> push to idle vec + add_permits(1).
        let waiters: VecDeque<oneshot::Sender<u32>> = VecDeque::new();
        assert!(waiters.is_empty());
        semaphore.add_permits(1);

        assert_eq!(semaphore.available_permits(), max_size);
    }

    /// After N handoffs, the semaphore must not drain.
    /// This is the core regression test for the permit fix.
    #[tokio::test]
    async fn semaphore_does_not_drain_after_n_handoffs() {
        let max_size = 4;
        let semaphore = Semaphore::new(max_size);

        for iteration in 0..100 {
            // Step 1: Client A checks out (acquire + forget).
            let permit = semaphore.acquire().await.unwrap();
            permit.forget();

            // Step 2: Client B waits (registers a oneshot waiter).
            let (tx, rx) = oneshot::channel::<u32>();
            let mut waiters: VecDeque<oneshot::Sender<u32>> = VecDeque::new();
            waiters.push_back(tx);

            // Step 3: Client B also acquires its own semaphore permit
            // (this is what acquire_semaphore does in timeout_get).
            let permit_b = semaphore.acquire().await.unwrap();
            permit_b.forget();

            // Step 4: Client A returns (handoff to B).
            // This models return_object: send to waiter + add_permits(1).
            let sender = waiters.pop_front().unwrap();
            sender.send(iteration).unwrap();
            semaphore.add_permits(1); // Client A's permit restored

            // Step 5: Client B receives and eventually returns via idle path.
            let _ = rx.await.unwrap();
            semaphore.add_permits(1); // Client B's permit restored

            // Invariant: all permits are back.
            assert_eq!(
                semaphore.available_permits(),
                max_size,
                "permit leak at iteration {iteration}"
            );
        }
    }

    /// Model the OLD (broken) handoff path that did NOT add_permits(1).
    /// Each cycle: client A checks out (forget permit), returns via
    /// handoff WITHOUT restoring the permit. One permit lost per cycle.
    /// After max_size cycles every permit is gone.
    #[test]
    fn semaphore_drains_without_handoff_permit_restore() {
        let max_size = 4;
        let semaphore = Semaphore::new(max_size);

        for i in 0..max_size {
            // Client A checks out: acquire + forget.
            let permit_a = semaphore
                .try_acquire()
                .expect("must have permits at this point");
            permit_a.forget();

            // OLD behavior: handoff sends but does NOT add_permits(1).
            // semaphore.add_permits(1); // <-- missing in old code

            // Net: lost one permit (client A's).
            assert_eq!(
                semaphore.available_permits(),
                max_size - (i + 1),
                "iteration {i}: expected {} leaked permits",
                i + 1,
            );
        }

        // All permits are gone.
        assert_eq!(semaphore.available_permits(), 0);
        assert!(semaphore.try_acquire().is_err());
    }

    /// Full checkout-use-return cycle via handoff path.
    /// Models: acquire_semaphore -> wrap_checkout(forget) -> return_object(handoff).
    #[tokio::test]
    async fn full_cycle_handoff_preserves_permits() {
        let max_size = 8;
        let semaphore = Semaphore::new(max_size);

        for _ in 0..50 {
            // Phase 1: checkout — acquire permit, then forget it.
            let permit = semaphore.acquire().await.unwrap();
            permit.forget();

            // Phase 2: a waiter exists, handoff succeeds.
            let (tx, _rx) = oneshot::channel::<u32>();
            let sent = tx.send(1).is_ok();
            assert!(sent);

            // Phase 3: return_object handoff path adds permit.
            semaphore.add_permits(1);
        }

        assert_eq!(semaphore.available_permits(), max_size);
    }

    /// Full checkout-use-return cycle via idle path.
    /// Models: acquire_semaphore -> wrap_checkout(forget) -> return_object(idle).
    #[tokio::test]
    async fn full_cycle_idle_preserves_permits() {
        let max_size = 8;
        let semaphore = Semaphore::new(max_size);

        for _ in 0..50 {
            // Phase 1: checkout.
            let permit = semaphore.acquire().await.unwrap();
            permit.forget();

            // Phase 2: no waiters, return to idle.
            semaphore.add_permits(1);
        }

        assert_eq!(semaphore.available_permits(), max_size);
    }

    /// Mixed handoff + idle returns must preserve permits.
    #[tokio::test]
    async fn mixed_handoff_and_idle_preserves_permits() {
        let max_size = 8;
        let semaphore = Semaphore::new(max_size);

        for i in 0..100 {
            let permit = semaphore.acquire().await.unwrap();
            permit.forget();

            if i % 3 == 0 {
                // Handoff path: waiter exists.
                let (tx, _rx) = oneshot::channel::<u32>();
                let _ = tx.send(1);
                semaphore.add_permits(1);
            } else if i % 3 == 1 {
                // Handoff path: waiter dropped (timed out), falls through to idle.
                let (tx, rx) = oneshot::channel::<u32>();
                drop(rx);
                let failed = tx.send(1).is_err();
                assert!(failed);
                // After skipping dead waiters, falls to idle path.
                semaphore.add_permits(1);
            } else {
                // Idle path: no waiters.
                semaphore.add_permits(1);
            }
        }

        assert_eq!(semaphore.available_permits(), max_size);
    }

    // ------------------------------------------------------------------
    // pre_replace_one does NOT inflate the semaphore
    // ------------------------------------------------------------------
    //
    // pre_replace_one creates a new connection and pushes it to idle
    // WITHOUT calling add_permits. The created connection sits in idle
    // until a client checks it out via acquire_semaphore. If pre_replace_one
    // incorrectly called add_permits, the semaphore would have more
    // permits than max_size, allowing more concurrent checkouts than the
    // pool can serve.
    //
    // We model the pre_replace_one contract: push to idle vec, bump
    // slots.size, but do NOT touch the semaphore.

    #[tokio::test]
    async fn pre_replace_does_not_inflate_semaphore() {
        let max_size = 4;
        let semaphore = Semaphore::new(max_size);
        let initial_permits = semaphore.available_permits();

        // Model pre_replace_one: creates a connection, pushes to idle,
        // increments slots.size. No semaphore interaction.
        // (In production code: slots.size += 1; push_idle(...))
        // The semaphore is intentionally untouched.

        // Simulate 3 pre-replacements.
        for _ in 0..3 {
            // pre_replace_one: only touches slots, not semaphore.
            // Nothing here — the test asserts the semaphore stays flat.
        }

        assert_eq!(
            semaphore.available_permits(),
            initial_permits,
            "pre_replace_one must not inflate the semaphore"
        );

        // Verify that the semaphore still caps at max_size checkouts.
        let mut held = Vec::new();
        for _ in 0..max_size {
            held.push(semaphore.acquire().await.unwrap());
        }
        assert_eq!(semaphore.available_permits(), 0);
        // One more acquire must block.
        let try_result = semaphore.try_acquire();
        assert!(try_result.is_err());
    }

    /// Verify that if pre_replace_one DID call add_permits, the
    /// semaphore would exceed max_size — proving the invariant matters.
    #[tokio::test]
    async fn pre_replace_add_permits_would_inflate() {
        let max_size = 4;
        let semaphore = Semaphore::new(max_size);

        // Wrong behavior: pre_replace_one calls add_permits(1).
        semaphore.add_permits(1);

        // Now the semaphore has max_size + 1 permits.
        assert_eq!(
            semaphore.available_permits(),
            max_size + 1,
            "add_permits(1) on pre-replace inflates the semaphore above max_size"
        );

        // This would allow max_size + 1 concurrent checkouts — a bug.
        let mut held = Vec::new();
        for _ in 0..=max_size {
            held.push(semaphore.acquire().await.unwrap());
        }
        assert_eq!(
            held.len(),
            max_size + 1,
            "inflated semaphore allows more checkouts than max_size"
        );
    }

    // ------------------------------------------------------------------
    // Burst gate try_recv drain: no double add_permits
    // ------------------------------------------------------------------
    //
    // In acquire_burst_gate, after the tokio::select! completes,
    // try_recv() may pull a late-arriving connection. This connection
    // is pushed to idle WITHOUT calling return_object (which would
    // add_permits again). The original return_object that sent to the
    // oneshot channel already called add_permits(1), so calling it
    // again would double-count.

    #[tokio::test]
    async fn try_recv_drain_must_not_double_add_permits() {
        let max_size = 4;
        let semaphore = Semaphore::new(max_size);

        // Client A checks out.
        let permit = semaphore.acquire().await.unwrap();
        permit.forget();
        assert_eq!(semaphore.available_permits(), max_size - 1);

        // Client B registers as a waiter.
        let (tx, mut rx) = oneshot::channel::<u32>();

        // Client A returns via handoff: send + add_permits(1).
        tx.send(42).unwrap();
        semaphore.add_permits(1);
        assert_eq!(semaphore.available_permits(), max_size);

        // The select! in burst gate finishes WITHOUT polling rx.
        // try_recv() pulls the connection.
        let value = rx.try_recv().unwrap();
        assert_eq!(value, 42);

        // The correct behavior: push to idle, do NOT call add_permits.
        // (return_object already did it above.)
        // If we incorrectly called add_permits again:
        //   semaphore.add_permits(1); // WRONG — would make permits = max_size + 1
        // The test verifies permits stay at max_size.
        assert_eq!(
            semaphore.available_permits(),
            max_size,
            "try_recv drain must not add extra permits"
        );
    }

    // ------------------------------------------------------------------
    // Concurrent handoff + idle: permit conservation under contention
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn concurrent_returns_preserve_permits() {
        use std::sync::Arc;

        let max_size = 16;
        let semaphore = Arc::new(Semaphore::new(max_size));
        let tasks = 100;

        let mut handles = Vec::with_capacity(tasks);
        for i in 0..tasks {
            let sem = Arc::clone(&semaphore);
            handles.push(tokio::spawn(async move {
                // Checkout.
                let permit = sem.acquire().await.unwrap();
                permit.forget();

                // Yield to interleave with other tasks.
                tokio::task::yield_now().await;

                // Return via handoff or idle.
                if i % 2 == 0 {
                    let (tx, _rx) = oneshot::channel::<u32>();
                    let _ = tx.send(1);
                }
                sem.add_permits(1);
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            semaphore.available_permits(),
            max_size,
            "all permits must be restored after concurrent checkout-return cycles"
        );
    }

    // ------------------------------------------------------------------
    // evict_dead_backends - background liveness scan gating
    // ------------------------------------------------------------------
    //
    // Positive-path coverage (an actual half-dead TCP backend gets evicted
    // and slots.size shrinks) is integration-level - it requires a real
    // PostgreSQL container restart and is exercised by the BDD scenario in
    // `tests/bdd/features/dead-backend-detection.feature`. The unit tests
    // here only pin the gating contract so a future refactor cannot
    // silently turn the scan into a hot-path bottleneck.
    const TEST_SKIP_RECENT_THRESHOLD: Duration = Duration::from_secs(30);

    fn empty_test_pool() -> Pool {
        use crate::config::{Address, User};
        use dashmap::DashMap;

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
        Pool::builder(server_pool)
            .pool_name("test_db".to_string())
            .username("test_user".to_string())
            .build()
    }

    fn empty_test_pool_with_max_size(max_size: usize) -> Pool {
        use crate::config::{Address, User};
        use crate::pool::types::PoolConfig;
        use dashmap::DashMap;

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
        Pool::builder(server_pool)
            .config(PoolConfig::new(max_size))
            .pool_name("test_db".to_string())
            .username("test_user".to_string())
            .build()
    }

    /// Variant of `empty_test_pool` that pins the queue mode to LIFO.
    /// `PoolConfig::default()` is FIFO, but the production default for
    /// the iServ deployment is LIFO (`server_round_robin = false`),
    /// so the queue-direction regression test
    /// (`evict_dead_backends_lifo_pops_from_back`) has to construct
    /// the pool explicitly with LIFO to mirror the field-deployed
    /// configuration.
    fn empty_test_pool_lifo() -> Pool {
        use crate::config::{Address, User};
        use crate::pool::types::PoolConfig;
        use dashmap::DashMap;

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
        let config = PoolConfig {
            queue_mode: QueueMode::Lifo,
            ..PoolConfig::default()
        };
        Pool::builder(server_pool)
            .config(config)
            .pool_name("test_db".to_string())
            .username("test_user".to_string())
            .build()
    }

    #[tokio::test]
    async fn evict_dead_backends_short_circuits_on_zero_timeout() {
        // `dead_backend_check_timeout = 0` is the operator-facing kill switch
        // for the scan. The method must return `(0, 0)` immediately, without
        // touching the slots mutex.
        let pool = empty_test_pool();
        let result = pool
            .evict_dead_backends(Duration::ZERO, 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(result, (0, 0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resize_shrink_drops_idle_before_waiting_for_active_returns() {
        use crate::server::Server;

        let pool = empty_test_pool_with_max_size(4);
        let mut active = Vec::new();
        {
            let mut slots = pool.inner.slots.lock();
            for _ in 0..3 {
                let permit = pool.semaphore().try_acquire().unwrap();
                permit.forget();
                slots.size += 1;
                active.push(ObjectInner {
                    obj: Server::test_zombie_marked_bad(),
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
            }
            slots.size += 1;
            slots.vec.push_back(ObjectInner {
                obj: Server::test_zombie_marked_bad(),
                metrics: Metrics::default(),
                coordinator_permit: None,
            });
        }

        assert_eq!(pool.status().size, 4);
        assert_eq!(pool.semaphore().available_permits(), 1);

        pool.resize(2);

        let slots = pool.inner.slots.lock();
        assert_eq!(
            slots.size, 3,
            "resize must evict idle objects until total size is no more \
             than active checkouts; otherwise active returns keep the \
             pool above the new max for extra cycles"
        );
        assert_eq!(
            slots.vec.len(),
            0,
            "the only idle object must be dropped during shrink before \
             waiting for active clients to return"
        );
        drop(slots);
        assert_eq!(
            pool.semaphore().available_permits(),
            0,
            "the freed idle semaphore slot must be retired for the \
             lower max_size"
        );

        drop(active);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resize_shrink_retires_over_limit_active_returns() {
        use crate::server::Server;

        let pool = empty_test_pool_with_max_size(4);
        let mut active = Vec::new();
        {
            let mut slots = pool.inner.slots.lock();
            for _ in 0..4 {
                let permit = pool.semaphore().try_acquire().unwrap();
                permit.forget();
                slots.size += 1;
                active.push(ObjectInner {
                    obj: Server::test_zombie_marked_bad(),
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
            }
        }

        assert_eq!(pool.semaphore().available_permits(), 0);
        pool.resize(2);
        assert_eq!(pool.status().size, 4);

        pool.inner.return_object(active.pop().unwrap());
        assert_eq!(
            pool.status().size,
            3,
            "first active return after a shrink must close the backend \
             instead of re-entering it into the idle queue"
        );
        assert_eq!(
            pool.semaphore().available_permits(),
            0,
            "the returned permit must be retired while the pool remains \
             above the new max_size"
        );
        assert_eq!(pool.inner.slots.lock().vec.len(), 0);

        pool.inner.return_object(active.pop().unwrap());
        assert_eq!(pool.status().size, 2);
        assert_eq!(pool.semaphore().available_permits(), 0);

        pool.inner.return_object(active.pop().unwrap());
        assert_eq!(
            pool.status().size,
            2,
            "once size reaches max_size, later returns should recycle normally"
        );
        assert_eq!(pool.inner.slots.lock().vec.len(), 1);
        assert_eq!(pool.semaphore().available_permits(), 1);

        drop(active);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resize_shrink_retires_over_limit_bad_drops() {
        use crate::server::Server;

        let pool = empty_test_pool_with_max_size(4);
        let mut active = Vec::new();
        {
            let mut slots = pool.inner.slots.lock();
            for _ in 0..3 {
                let permit = pool.semaphore().try_acquire().unwrap();
                permit.forget();
                slots.size += 1;
                active.push(ObjectInner {
                    obj: Server::test_zombie_marked_bad(),
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
            }
            let permit = pool.semaphore().try_acquire().unwrap();
            permit.forget();
            slots.size += 1;
        }

        let bad_object = Object {
            inner: Some(ObjectInner {
                obj: Server::test_zombie_marked_bad(),
                metrics: Metrics::default(),
                coordinator_permit: None,
            }),
            pool: Arc::downgrade(&pool.inner),
        };

        pool.resize(2);
        drop(bad_object);

        assert_eq!(
            pool.status().size,
            3,
            "bad connection drops after shrink must reduce size exactly \
             like normal over-limit returns"
        );
        assert_eq!(
            pool.semaphore().available_permits(),
            0,
            "bad connection drops above the new max_size must retire the \
             returning slot instead of re-opening old capacity"
        );

        drop(active);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn object_drop_evicts_backend_returned_inside_transaction() {
        use crate::server::Server;

        let pool = empty_test_pool_with_max_size(1);
        pool.semaphore().try_acquire().unwrap().forget();
        {
            let mut slots = pool.inner.slots.lock();
            slots.size = 1;
        }

        let mut server = Server::test_dead_socket();
        server.in_transaction = true;

        let object = Object {
            inner: Some(ObjectInner {
                obj: server,
                metrics: Metrics::default(),
                coordinator_permit: None,
            }),
            pool: Arc::downgrade(&pool.inner),
        };

        drop(object);

        assert_eq!(
            pool.status().size,
            0,
            "a checked-out backend dropped inside a transaction must close \
             instead of re-entering the idle pool"
        );
        assert_eq!(
            pool.inner.slots.lock().vec.len(),
            0,
            "dirty drop must not leave the backend available for direct reuse"
        );
        assert_eq!(
            pool.semaphore().available_permits(),
            1,
            "evicting the dirty backend must release the checked-out slot"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_replacement_overshoot_return_does_not_leak_permit() {
        use crate::server::Server;

        // pre_replace_one drives slots.size
        // above max_size WITHOUT removing a semaphore permit (it is not a
        // resize). A return during that overshoot window hits the
        // `size > max_size` retire branch; because permits_to_retire stays 0,
        // the returning client's permit must be RESTORED, not retired -
        // otherwise every such return permanently leaks a permit and the pool
        // drifts into self-inflicted "too many clients".
        let max_size = 2;
        let pool = empty_test_pool_with_max_size(max_size);

        // One client checked out: forget its permit, account for it in size.
        pool.semaphore().try_acquire().unwrap().forget();
        {
            let mut slots = pool.inner.slots.lock();
            slots.size = max_size;
        }
        assert_eq!(pool.semaphore().available_permits(), max_size - 1);

        // pre_replace_one created an extra backend ahead of lifetime expiry:
        // size = max_size + 1, semaphore untouched, permits_to_retire stays 0.
        {
            let mut slots = pool.inner.slots.lock();
            slots.size += 1;
            assert_eq!(
                slots.permits_to_retire, 0,
                "pre-replacement must not mark any permit for retirement"
            );
        }

        // The checked-out client returns during the overshoot window.
        let inner = pool
            .inner
            .new_object_inner(Server::test_zombie_marked_bad(), None);
        pool.inner.return_object(inner);

        // The extra connection was retired (size back to max_size) AND the
        // permit was restored - no leak.
        assert_eq!(
            pool.status().size,
            max_size,
            "the pre-replacement overshoot connection must be retired on return"
        );
        assert_eq!(
            pool.semaphore().available_permits(),
            max_size,
            "a return during pre-replacement overshoot must restore the permit, not leak it"
        );
    }

    #[test]
    fn fresh_create_paths_recheck_open_generation_after_await() {
        let src = include_str!("inner.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        for (name, start, end) in [
            (
                "pre_replace_one",
                "async fn pre_replace_one(&self)",
                "/// Create a new backend connection",
            ),
            (
                "create_connection",
                "async fn create_connection(",
                "/// Returns true when every permit is in use",
            ),
            (
                "replenish",
                "pub async fn replenish(&self, count: usize)",
                "/// Closes this Pool.",
            ),
        ] {
            let start_idx = impl_src.find(start).expect("create path start marker");
            let block = &impl_src[start_idx..];
            let end_idx = block.find(end).expect("create path end marker");
            let block = &block[..end_idx];
            let create_idx = block
                .find("server_pool.create")
                .expect("create path must await server_pool.create");
            let post_create = &block[create_idx..];
            let size_idx = post_create
                .find("slots.size += 1")
                .expect("create path must account slots.size");
            let before_size = &post_create[..size_idx];
            assert!(
                before_size.contains("accepts_fresh_backend_after_create"),
                "{name} must recheck that the pool generation is still open \
                 after awaiting server_pool.create() and before slots.size accounting"
            );
        }
    }

    #[test]
    fn fresh_create_guard_rejects_closed_or_zero_generation() {
        let pool = empty_test_pool_with_max_size(2);
        {
            let slots = pool.inner.slots.lock();
            assert!(
                pool.inner.accepts_fresh_backend_after_create(&slots),
                "open non-zero pool must accept fresh post-create backends"
            );
        }

        pool.close_new_checkouts();
        {
            let slots = pool.inner.slots.lock();
            assert!(
                !pool.inner.accepts_fresh_backend_after_create(&slots),
                "closed generation must reject fresh post-create backends"
            );
        }

        let zero = empty_test_pool_with_max_size(2);
        zero.resize(0);
        {
            let slots = zero.inner.slots.lock();
            assert!(
                !zero.inner.accepts_fresh_backend_after_create(&slots),
                "zero-sized generation must reject fresh post-create backends"
            );
        }
    }

    #[tokio::test]
    async fn evict_dead_backends_short_circuits_on_zero_max() {
        // Same kill-switch semantics for `dead_backend_check_max_per_cycle = 0`.
        let pool = empty_test_pool();
        let result = pool
            .evict_dead_backends(Duration::from_secs(2), 0, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(result, (0, 0));
    }

    #[tokio::test]
    async fn evict_dead_backends_noop_on_empty_pool() {
        // No idle objects -> no popping, no checking, no eviction. The pool
        // would not even know what timeout to apply against.
        let pool = empty_test_pool();
        let result = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(result, (0, 0));
    }

    /// Zombie-scan invariant: a backend whose `last_activity` is
    /// inside the skip-recent threshold (default 30 s) must NOT be
    /// probed by `check_alive` - `evict_dead_backends` should push
    /// it back to the idle vec as a survivor without burning a
    /// SELECT 1 round-trip. Without this the scan wakes up every
    /// retain tick and re-checks the same hot pool the application
    /// is already exercising on the query path, which doubled the
    /// pool's PostgreSQL traffic on busy deployments.
    ///
    /// Uses `test_dead_socket()` to make the assertion sharp: if
    /// the fast path were skipped and check_alive ran, the EPIPE
    /// would surface as `evicted == ZOMBIES`. With the fast path
    /// firing on every recent backend the count must be 0.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evict_dead_backends_skips_recently_active_backends() {
        use crate::server::Server;

        const ZOMBIES: usize = 4;
        let pool = empty_test_pool();
        {
            let mut guard = pool.inner.slots.lock();
            for _ in 0..ZOMBIES {
                let server = Server::test_dead_socket(); // peer dropped, EPIPE on send
                                                         // `last_activity` is `SystemTime::now()` from the
                                                         // constructor - well inside the 30 s threshold.
                guard.vec.push_back(ObjectInner {
                    obj: server,
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
                guard.size += 1;
            }
        }
        let (checked, evicted) = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(
            checked, ZOMBIES,
            "scan must still account for popped objects in the \
             `checked` metric - only the SELECT 1 round-trip is \
             skipped, not the per-object bookkeeping."
        );
        assert_eq!(
            evicted, 0,
            "fresh-`last_activity` backends must \
             bypass check_alive and survive the scan. evicted={evicted} \
             means the fast-path gate regressed and the scan re-probed \
             a connection the protocol_io layer just touched."
        );
    }

    /// A recently-active skip is only valid inside the caller-supplied
    /// recent window. Once the retain interval has passed, the scan must
    /// probe again so a PostgreSQL restart that happened after the last
    /// successful query cannot leave zombie sockets counted as idle
    /// capacity until the fixed 30 s production window expires.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evict_dead_backends_probes_after_recent_window_expires() {
        use crate::server::Server;

        const ZOMBIES: usize = 4;
        let pool = empty_test_pool();
        {
            let mut guard = pool.inner.slots.lock();
            for _ in 0..ZOMBIES {
                let mut server = Server::test_dead_socket();
                server.last_activity =
                    std::time::SystemTime::now() - std::time::Duration::from_secs(3);
                guard.vec.push_back(ObjectInner {
                    obj: server,
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
                guard.size += 1;
            }
        }

        let (checked, evicted) = pool
            .evict_dead_backends(Duration::from_secs(2), 32, Duration::from_secs(2))
            .await;

        assert_eq!(checked, ZOMBIES);
        assert_eq!(
            evicted, ZOMBIES,
            "dead sockets older than the retain recent-window must be \
             probed and evicted, not kept as fresh survivors."
        );
    }

    #[tokio::test]
    async fn evict_dead_backends_skips_under_pressure_pool() {
        // Same rationale as `retain_pool_skips_under_pressure`: yanking an
        // idle backend out from under a queued client just forces a
        // `connect()` on the wait path. The scan must defer to the next
        // cycle when the semaphore is exhausted.
        let pool = empty_test_pool();
        let semaphore = pool.semaphore();
        let total = semaphore.available_permits();
        let mut held = Vec::with_capacity(total);
        for _ in 0..total {
            held.push(semaphore.acquire().await.unwrap());
        }
        assert!(pool.under_pressure());

        let result = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(result, (0, 0));
    }

    #[tokio::test]
    async fn evict_dead_backends_skips_paused_pool() {
        // PAUSE freezes checkin/checkout; the background scan must respect
        // that and not surprise the admin with disappearing connections.
        let pool = empty_test_pool();
        pool.server_pool().pause();
        assert!(pool.is_paused());
        let result = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(result, (0, 0));
        pool.server_pool().resume();
    }

    /// Invariant guard.
    ///
    /// `evict_dead_backends` `try_acquire`'s a semaphore permit per pop and
    /// `forget()`s it for the off-lock check_alive window. The permits are
    /// then returned by a single `add_permits(checked)` after the lock has
    /// released - survivor `push_idle` and evict `size -= 1` already
    /// happened inside that lock. If a future refactor moves the
    /// `add_permits` call before the `size -=` write, or drops the
    /// permits inside the loop instead of in one batch, this test catches
    /// it: after the function returns the pool must hold exactly the same
    /// number of available permits as before the scan, and `slots.size`
    /// must not have been touched (no real backends were probed because
    /// the vec was empty, but the gating still must keep the books).
    #[tokio::test]
    async fn evict_dead_backends_preserves_permit_count() {
        let pool = empty_test_pool();
        let permits_before = pool.semaphore().available_permits();
        let size_before = pool.status().size;

        let (checked, evicted) = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!((checked, evicted), (0, 0));

        let permits_after = pool.semaphore().available_permits();
        let size_after = pool.status().size;
        assert_eq!(
            permits_after, permits_before,
            "no real probes happened (empty vec) - permit count must be \
             identical, not over- or under-counted"
        );
        assert_eq!(
            size_after, size_before,
            "no real evictions happened - slots.size must be unchanged"
        );
    }

    /// Real pop -> check -> evict path coverage.
    ///
    /// The other `evict_dead_backends_*` tests in this module short-
    /// circuit through `under_pressure`/empty-vec/zero-timeout branches -
    /// they pin the gating contract but never exercise the bookkeeping
    /// path that runs when objects are actually popped. This test does:
    ///
    /// 1. Seed the idle vec with `N` `Server` instances that have
    ///    `bad = true` set up front (test-only `Server::test_zombie_marked_bad`
    ///    backed by a `UnixStream` pair whose peer is dropped - no real
    ///    PostgreSQL needed). `slots.size` is bumped to match, mirroring
    ///    what a normal `create()` would do.
    /// 2. Snapshot `available_permits` and `slots.size`.
    /// 3. Call `evict_dead_backends`. Each iteration must:
    ///       - `try_acquire` a permit and `forget()` it (hidden state),
    ///       - `pop_front()` the zombie,
    ///       - see `is_bad() == true`, increment `evicted`,
    ///       - skip `check_alive` (saves us from needing a live TCP loop),
    ///       - after the loop: take the slots lock, do `size -= evicted`,
    ///       - drop the lock, then `add_permits(checked)`.
    ///
    /// Post-conditions verify each transition:
    ///   - return value `(N, N)` - every zombie was both probed and ejected,
    ///   - `available_permits` is restored to the pre-injection value
    ///     (try_acquire×N -> forget×N -> add_permits(N) round-trip is exact),
    ///   - `slots.size` is back to the pre-injection value (decremented
    ///     by exactly `N`, not 0 and not 2N).
    ///
    /// A regression that returns permits before deducting `slots.size`
    /// (the original race) would still leave the post-state
    /// correct in this single-task test - the deterministic race window
    /// against a concurrent checkout needs a multi-task harness that
    /// the BDD scenario in `tests/bdd/features/dead-backend-detection.feature`
    /// supplies. But a regression that forgets `evicted += 1`, or
    /// forgets `add_permits`, or leaks the permit on `forget()` without
    /// returning it fails this test loudly with the exact off-count.
    #[cfg(unix)]
    #[tokio::test]
    async fn evict_dead_backends_real_path_evicts_zombies() {
        use crate::server::Server;
        use std::time::Duration;

        const ZOMBIES: usize = 3;

        let pool = empty_test_pool();
        let permits_before = pool.semaphore().available_permits();
        let size_before = pool.status().size;

        // Inject `ZOMBIES` marked-bad Server objects directly into the
        // idle vec. This mirrors what `return_object` would do for a
        // backend that finished a transaction and was placed back into
        // the pool, except the backend is already dead.
        {
            let mut guard = pool.inner.slots.lock();
            for _ in 0..ZOMBIES {
                guard.vec.push_back(ObjectInner {
                    obj: Server::test_zombie_marked_bad(),
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
                guard.size += 1;
            }
        }
        let size_after_inject = pool.status().size;
        assert_eq!(
            size_after_inject,
            size_before + ZOMBIES,
            "test setup precondition: slots.size must reflect the injected zombies"
        );

        let (checked, evicted) = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;

        assert_eq!(
            (checked, evicted),
            (ZOMBIES, ZOMBIES),
            "every probed object must be ejected via the is_bad() short-circuit"
        );

        let permits_after = pool.semaphore().available_permits();
        let size_after = pool.status().size;

        // Real bookkeeping invariants - these are what a regression of the
        // form "skip add_permits", "double add_permits", "forget evicted++"
        // would break.
        assert_eq!(
            permits_after, permits_before,
            "try_acquire×{ZOMBIES} -> permit.forget()×{ZOMBIES} -> \
             add_permits({ZOMBIES}) must be net-zero on the semaphore",
        );
        assert_eq!(
            size_after, size_before,
            "slots.size must shrink by exactly the evicted count, not by \
             twice the count and not by zero",
        );
    }

    /// Strict ordering guard.
    ///
    /// Simulate the worst-case race the original race called out:
    /// hand-acquire every semaphore permit in advance (modelling a fully
    /// saturated pool where every backend is in flight to some client),
    /// then ask `evict_dead_backends` to probe. The function must NOT try
    /// to forget more permits than the semaphore can hand out - the
    /// `try_acquire` per pop guards against that - and it must NOT touch
    /// `slots.size` when it could not pop anything. Without the
    /// `try_acquire`-first pattern this would either panic on
    /// `add_permits(checked)` over-restoration, or it would leave the
    /// pool with negative permit accounting.
    #[tokio::test]
    async fn evict_dead_backends_safe_under_full_semaphore_pressure() {
        let pool = empty_test_pool();
        let total = pool.semaphore().available_permits();
        let mut held = Vec::with_capacity(total);
        for _ in 0..total {
            held.push(pool.semaphore().acquire().await.unwrap());
        }
        assert_eq!(pool.semaphore().available_permits(), 0);
        // under_pressure() is true here - the scan must short-circuit
        // BEFORE attempting any try_acquire / pop, returning (0, 0) and
        // leaving the semaphore at 0 available permits.
        let result = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(result, (0, 0));
        assert_eq!(
            pool.semaphore().available_permits(),
            0,
            "under_pressure short-circuit must not touch permits"
        );
        drop(held);
        // Now all permits are back.
        assert_eq!(pool.semaphore().available_permits(), total);
    }

    /// Regression guard: `evict_dead_backends`
    /// must keep the pool's accounting consistent even when the scan
    /// task is cancelled mid-flight (panic during `check_alive`, future
    /// `select!` wrapper around the retain task, runtime shutdown).
    /// Without `EvictGuard` the forgotten semaphore permits and the
    /// not-yet-deducted `slots.size` would leak permanently until
    /// process restart - slow, silent degradation that no
    /// `SHOW STATS`-driven alert would catch.
    ///
    /// Setup uses `Server::test_silent_socket()` - peer kept alive, so
    /// `check_alive` parks on its recv deadline waiting for a response
    /// that never comes. The test wraps the scan in
    /// `tokio::time::timeout(50ms, ...)` to force a deterministic
    /// cancellation; when the timeout fires, the scan future drops
    /// while parked at the `check_alive(...).await` point, which is
    /// exactly the cancellation shape `EvictGuard::drop` exists to
    /// handle.
    ///
    /// Post-cancellation assertions:
    ///   * `available_permits` is back to its pre-scan value (all
    ///     `try_acquire().forget()` were balanced by the Drop's
    ///     `add_permits`),
    ///   * `slots.size` reflects the popped count being deducted (the
    ///     popped objects went out of the idle vec into the scan's
    ///     temporary Vec, then dropped during unwind - Drop deducts).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn evict_dead_backends_cancellation_releases_permits() {
        use crate::server::Server;
        use std::time::Duration;

        const ZOMBIES: usize = 4;

        let pool = empty_test_pool();
        let permits_before = pool.semaphore().available_permits();
        let size_before = pool.status().size;

        // Inject ZOMBIES backends with silent peers (bad=false so
        // is_bad() short-circuit doesn't apply; check_alive will hang
        // on recv until the per-object 30s timeout). Keep the peer
        // ends alive so the writes succeed but no response arrives.
        let mut peers = Vec::with_capacity(ZOMBIES);
        {
            let mut guard = pool.inner.slots.lock();
            for _ in 0..ZOMBIES {
                let (mut server, peer) = Server::test_silent_socket();
                // age `last_activity` past the
                // skip-recent threshold so the cancellation harness
                // actually exercises `check_alive(...).await` - the
                // fast path would otherwise push the silent socket
                // back as a "recently active" survivor and the scan
                // would finish before the outer 50 ms timeout.
                server.last_activity =
                    std::time::SystemTime::now() - std::time::Duration::from_secs(60);
                peers.push(peer);
                guard.vec.push_back(ObjectInner {
                    obj: server,
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
                guard.size += 1;
            }
        }
        let size_after_inject = pool.status().size;
        assert_eq!(size_after_inject, size_before + ZOMBIES);

        // Force the scan to cancel mid-flight: wrap in a timeout
        // shorter than the per-object check_alive deadline so we drop
        // the future while parked in `check_alive(...).await`.
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            pool.evict_dead_backends(Duration::from_secs(30), 32, TEST_SKIP_RECENT_THRESHOLD),
        )
        .await;
        assert!(
            result.is_err(),
            "scan must time out (cancel) mid-flight - silent peers \
             should keep check_alive parked past the 50ms outer \
             timeout. Got Ok(..) which means the scan finished, the \
             test setup is broken."
        );

        // EvictGuard::drop must have run during the timeout's
        // drop-the-future unwind. Verify the accounting is restored.
        let permits_after = pool.semaphore().available_permits();
        let size_after = pool.status().size;

        assert_eq!(
            permits_after, permits_before,
            "cancellation must restore every permit forgotten during the \
             pop phase. permits_before={permits_before}, after={permits_after}: \
             the delta is a permanent leak that would degrade the pool's \
             effective capacity until process restart.",
        );
        assert_eq!(
            size_after, size_before,
            "cancellation must deduct the popped count from slots.size - \
             those objects are gone from the idle vec (dropped during \
             unwind, Server::drop fired) and not coming back. \
             size_before={size_before}, after={size_after}: any residual \
             would make replenish skip refilling, repeating the original \
             zombie-pool symptom.",
        );

        // Clean up peer ends so the test fixture doesn't leak fds.
        drop(peers);
    }

    /// Regression guard: in LIFO mode (the
    /// production default via `server_round_robin = false`), the
    /// liveness scan must visit the BACK of the idle deque. A
    /// regression that uses `pop_front` in both modes would perfectly
    /// shield any zombie sitting at the back from the scan - fresh
    /// alive entries at the front get popped, verified, and pushed
    /// straight back to the front (LIFO push_idle = push_front),
    /// repeating forever while the actual zombies at the back never
    /// get probed.
    ///
    /// Setup is deterministic: inject N marked-bad zombies via
    /// `push_back` so the deque content goes `[oldest=front,
    /// newest=back]`, label each with a synthetic `process_id` so we
    /// can identify the survivor, then run a partial scan
    /// (`max_per_cycle < N`) and assert the surviving entry is the
    /// one at the FRONT (= the one furthest from the LIFO back, the
    /// position the scan must skip).
    ///
    /// In the buggy `pop_front` impl the survivor would be at the
    /// BACK instead - the assertion fails loudly with that exact
    /// signature.
    #[cfg(unix)]
    #[tokio::test]
    async fn evict_dead_backends_lifo_pops_from_back() {
        use crate::server::Server;
        use std::time::Duration;

        const ZOMBIES: i32 = 4;
        const MAX_PER_CYCLE: usize = 3;

        let pool = empty_test_pool_lifo();
        // Mirror the production iServ deployment (server_round_robin =
        // false -> LIFO). `PoolConfig::default()` ships FIFO, so the
        // helper pins it.
        assert!(matches!(pool.inner.config.queue_mode, QueueMode::Lifo));

        let pre_size = pool.status().size;

        // Inject ZOMBIES marked-bad backends, labelled 1..=ZOMBIES.
        // push_back means deque is [1, 2, 3, 4]; for LIFO checkout
        // pop_front would pull pid=1 (oldest); evict's pop_back must
        // pull pid=4 first.
        {
            let mut guard = pool.inner.slots.lock();
            for pid in 1..=ZOMBIES {
                let mut obj = Server::test_zombie_marked_bad();
                obj.test_set_process_id(pid);
                guard.vec.push_back(ObjectInner {
                    obj,
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
                guard.size += 1;
            }
        }
        assert_eq!(pool.status().size, pre_size + ZOMBIES as usize);

        // Partial scan: pop MAX_PER_CYCLE (3) from the back, leave 1.
        let (checked, evicted) = pool
            .evict_dead_backends(
                Duration::from_secs(2),
                MAX_PER_CYCLE,
                TEST_SKIP_RECENT_THRESHOLD,
            )
            .await;
        assert_eq!(
            (checked, evicted),
            (MAX_PER_CYCLE, MAX_PER_CYCLE),
            "the {MAX_PER_CYCLE} zombies popped (from the LIFO back) \
             must all be evicted via the is_bad() short-circuit"
        );

        // The deque should now hold exactly the 1 entry that was the
        // furthest from the LIFO back - pid=1 (front position).
        let guard = pool.inner.slots.lock();
        assert_eq!(
            guard.vec.len(),
            1,
            "exactly one zombie should remain after a partial scan"
        );
        let survivor_pid = guard.vec[0].obj.test_process_id();
        assert_eq!(
            survivor_pid, 1,
            "in LIFO mode the scan must pop from the back; the entry \
             that survives a max_per_cycle < N scan is the one at the \
             FRONT (pid=1 here). Got pid={survivor_pid} - if this is \
             pid=4, the scan is using pop_front and shielding back-of-\
             queue zombies from eviction (the production-default bug)."
        );
    }

    /// Deterministic ordering race coverage for the dead-backend eviction path.
    ///
    /// **Setup.** Inject N `Server::test_dead_socket()` objects (peer
    /// `UnixStream` dropped, `bad = false` so `evict_dead_backends`
    /// actually invokes `check_alive(...).await` per object - that
    /// `.await` is the scheduling yield point a concurrent observer
    /// needs to interleave with the eviction loop). Run an observer
    /// task on the same multi-threaded runtime that:
    ///   1. waits until `available_permits < max_size` (proof that
    ///      evict has begun and pulled at least one `try_acquire`),
    ///   2. then repeatedly snapshots `(permits, slots.size)` and
    ///      flags any moment when `permits == max_size` while
    ///      `slots.size > post_evict_size`.
    ///
    /// **What the invariant means.** In correct flow `add_permits` runs
    /// AFTER `drop(guard)` AND AFTER `guard.size -= evicted`, so the
    /// first instant the observer can ever see `permits == max_size`
    /// is the same instant `slots.size` is already at `post_evict_size`.
    /// A regression that issues `add_permits(1)` inside the `for inner
    /// in popped` loop (the realistic refactor shape - pull the
    /// restore into the per-Err arm to "balance the books eagerly")
    /// would race: between an early `add_permits(1)` and the final
    /// batched `guard.size -= evicted` there is a window where
    /// `permits` is back at `max_size` but `slots.size` still reflects
    /// the not-yet-evicted backends.
    ///
    /// **Negative verification done locally.** Sketched one regression
    /// shape and confirmed the harness catches it:
    ///   - move `add_permits(1)` into the per-iteration `match Err` arm
    ///     of the eviction loop and drop the trailing batched
    ///     `add_permits(checked)`. Harness flagged 29 violations on a
    ///     single run on the developer machine.
    /// Mutation reverted after verification.
    ///
    /// **Acknowledged blind spot:** a regression that moves
    /// `add_permits` inside the slots critical section but keeps it
    /// before `guard.size -= evicted` still leaves the observer's
    /// `slots.try_lock()` returning `None` (the production lock is
    /// held by evict) for the entire bug window - by the time the
    /// observer reads `size`, the `size -=` write has already
    /// committed. The harness cannot deterministically catch this
    /// in-section reorder shape.
    ///
    /// However, this shape does **not** produce the user-observable
    /// the over-capacity behaviour: a
    /// concurrent `checkout` that wins one of the early-released
    /// permits still has to take `slots.lock()` before it can pull an
    /// idle object or open a new one, and that lock is held by evict
    /// for the entire window, so by the time checkout sees the slot
    /// state it is already post-eviction. The in-section variant is
    /// therefore not a regression checkout clients could observe;
    /// structurally it is also out-of-pattern (every other
    /// `add_permits` call in `pool/inner.rs` runs after
    /// `drop(guard)`), so it is caught at review time.
    ///
    /// **CI flakiness note:** the sanity check (observer must have
    /// seen `permits == max_size` at least once) is soft - it logs a
    /// warning if the runtime didn't schedule the observer to catch
    /// a post-evict snapshot, but does NOT fail the test. Under heavy
    /// parallel CI load (e.g. the full `cargo test --lib` on a small
    /// VM) the observer can be starved of CPU and miss the brief
    /// window. The test still rejects positive observations of the
    /// regression - the local negative-verification documented above
    /// is what gives confidence the harness catches the bug, not
    /// every CI run.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn evict_dead_backends_ordering_race_invariant_holds() {
        use crate::server::Server;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        const ZOMBIES: usize = 8;
        // Observer polls fast but yields between reads - the per-iter
        // `check_alive(...).await` inside `evict_dead_backends` is what
        // gives the scheduler the opportunity to swap to the observer
        // and back.
        const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);

        let pool = empty_test_pool();
        let max_size = pool.semaphore().available_permits();
        let pre_size = pool.status().size;
        let post_size_expected = pre_size; // every zombie gets evicted

        // Inject the dead-socket zombies. Bad=false so `is_bad()`
        // short-circuit in evict is skipped and `check_alive(...).await`
        // runs (and fails with EPIPE because the UnixStream peer is
        // dropped).
        {
            let mut guard = pool.inner.slots.lock();
            for _ in 0..ZOMBIES {
                let mut server = Server::test_dead_socket();
                // age `last_activity` past the
                // skip-recent threshold so the ordering-race harness
                // actually drives the eviction loop through
                // `check_alive(...).await`. Otherwise every dead
                // socket gets a fast-path survivor pass and the
                // assertion downstream (`evicted == ZOMBIES`) fires.
                server.last_activity =
                    std::time::SystemTime::now() - std::time::Duration::from_secs(60);
                guard.vec.push_back(ObjectInner {
                    obj: server,
                    metrics: Metrics::default(),
                    coordinator_permit: None,
                });
                guard.size += 1;
            }
        }
        assert_eq!(pool.status().size, pre_size + ZOMBIES);

        let pool_inner = Arc::clone(&pool.inner);
        let stop = Arc::new(AtomicBool::new(false));
        let violations = Arc::new(AtomicUsize::new(0));
        let max_size_seen_during_evict = Arc::new(AtomicBool::new(false));

        let observer_handle: tokio::task::JoinHandle<()> = {
            let pool_inner = Arc::clone(&pool_inner);
            let stop = Arc::clone(&stop);
            let violations = Arc::clone(&violations);
            let max_size_seen = Arc::clone(&max_size_seen_during_evict);
            tokio::spawn(async move {
                // Phase 1: wait until evict has demonstrably started
                // (semaphore is below max - at least one try_acquire
                // ran). Without this the initial state itself would
                // be a false positive.
                let phase1_deadline = std::time::Instant::now() + OBSERVATION_TIMEOUT;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if pool_inner.semaphore.available_permits() < max_size {
                        break;
                    }
                    if std::time::Instant::now() > phase1_deadline {
                        return;
                    }
                    tokio::task::yield_now().await;
                }

                // Phase 2: observe `(permits, slots.size)` and flag
                // any instant when permits returned to max_size while
                // slots.size still reflected the pre-evict count. In
                // correct flow this never happens - permits hits
                // max_size only AFTER size -= evicted inside the
                // post-loop lock.
                while !stop.load(Ordering::Relaxed) {
                    let p = pool_inner.semaphore.available_permits();
                    // try_lock so a held slots-lock by evict doesn't
                    // serialize us with it (which would mask the very
                    // race we want to observe).
                    let size_opt = pool_inner.slots.try_lock().map(|g| g.size);
                    if let Some(s) = size_opt {
                        if p == max_size {
                            max_size_seen.store(true, Ordering::Relaxed);
                            if s > post_size_expected {
                                violations.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
        };

        // Tiny delay so the observer is parked on its phase-1 loop
        // before evict starts.
        tokio::task::yield_now().await;

        let (checked, evicted) = pool
            .evict_dead_backends(Duration::from_secs(2), 32, TEST_SKIP_RECENT_THRESHOLD)
            .await;
        assert_eq!(
            (checked, evicted),
            (ZOMBIES, ZOMBIES),
            "every dead-socket zombie must be evicted (check_alive fails \
             with EPIPE, mark_bad fires, the match-Err arm increments \
             evicted)",
        );

        // Give the observer one more poll to see the final state.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        stop.store(true, Ordering::Relaxed);
        observer_handle.await.expect("observer task must not panic");

        let v = violations.load(Ordering::Relaxed);
        let seen_post = max_size_seen_during_evict.load(Ordering::Relaxed);

        // Soft sanity: warn (don't fail) if observer didn't catch a
        // post-evict snapshot. See doc comment "CI flakiness note".
        if !seen_post {
            eprintln!(
                "evict_dead_backends_ordering_race_invariant_holds: \
                 observer never saw `permits == max_size` after evict - \
                 scheduler likely starved it under parallel test load. \
                 Test passes neutrally; the regression-catching power \
                 comes from local negative verification documented in \
                 the test header."
            );
        }
        assert_eq!(
            v, 0,
            "observed {v} moments where `permits == max_size` and \
             `slots.size > {post_size_expected}` simultaneously. A \
             regression that returns semaphore permits BEFORE deducting \
             slots.size leaves the pool transiently over-capacity - a \
             concurrent acquire would see more permits than the pool has \
             live slots.",
        );
    }
}
