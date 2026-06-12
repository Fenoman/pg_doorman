use crate::errors::Error;
/// Handle clients by pretending to be a PostgreSQL server.
use ahash::{AHashMap, AHashSet};
use bytes::BytesMut;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::BufReader;

use crate::client::buffer_pool::PooledBuffer;
use crate::config::config_arc;
use crate::messages::{error_response_timeout, Parse};
use crate::pool::{get_pool_by_id, ClientServerMap, ConnectionPool, PoolIdentifier};
use crate::server::cleanup::{ResetCleanupCommand, SetCleanupCommand};
use crate::server::ServerParameters;
use crate::stats::{ClientStats, PreparedCacheSnapshot, ServerStats};

/// Key for prepared statement cache - avoids string allocations for anonymous statements
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PreparedStatementKey {
    /// Named prepared statement (client-provided name)
    Named(String),
    /// Anonymous prepared statement (identified by hash)
    Anonymous(u64),
}

impl PreparedStatementKey {
    /// Create a key from client-given name, using hash for anonymous statements
    #[inline]
    pub fn from_name_or_hash(name: String, hash: u64) -> Self {
        if name.is_empty() {
            PreparedStatementKey::Anonymous(hash)
        } else {
            PreparedStatementKey::Named(name)
        }
    }
}

/// Borrowed view over a `PreparedStatementKey`. Used by `PreparedStatementCache::iter`
/// to yield key kinds without cloning the Named String per entry. The hot path
/// (`cache_memory_usage`, called from `update_prepared_cache_stats` after every Parse)
/// only inspects the kind and reads the borrowed `&str` — owning the key would
/// allocate a fresh String per yield.
#[derive(Debug, Clone, Copy)]
pub enum PreparedStatementKeyRef<'a> {
    /// Named prepared statement (client-provided name)
    Named(&'a str),
    /// Anonymous prepared statement (identified by hash)
    Anonymous(u64),
}

impl<'a> From<&'a PreparedStatementKey> for PreparedStatementKeyRef<'a> {
    fn from(key: &'a PreparedStatementKey) -> Self {
        match key {
            PreparedStatementKey::Named(name) => PreparedStatementKeyRef::Named(name.as_str()),
            PreparedStatementKey::Anonymous(hash) => PreparedStatementKeyRef::Anonymous(*hash),
        }
    }
}

/// Per-client prepared statement cache, split into two parts:
///   - `named`: AHashMap of client-provided statement names. Never evicted
///     by the pooler; lifecycle is owned by the client (Close, DEALLOCATE,
///     disconnect).
///   - `anonymous`: LRU keyed by query hash. Bounded by
///     `client_anonymous_prepared_cache_size`. On eviction the local
///     `Arc<Parse>` is dropped; nothing is sent to the backend.
pub struct PreparedStatementCache {
    named: AHashMap<String, CachedStatement>,
    anonymous: AnonymousCache,
    /// parallel insertion-order queue for the Named map.
    /// `AHashMap::keys().next()` returns hash-bucket-order, NOT
    /// insertion-order - under cap pressure that picked an
    /// effectively-random victim (could evict the just-inserted hot
    /// statement). A `VecDeque<String>` of names mirrors the insertion
    /// order so eviction is genuine FIFO without the parallel-cost of
    /// a full LRU.
    named_order: std::collections::VecDeque<String>,
    /// incremental mirror of the per-entry fixed overhead.
    ///
    /// The previous `cache_memory_usage()` implementation walked every
    /// Named + Anonymous entry on every Parse - an O(N) hot path that, at
    /// 10k Parse/sec with the default 8192-entry Anonymous LRU, costs
    /// ~80M atomic loads/sec via `Arc::strong_count`. The new counter is
    /// updated in `put`/`pop`/`clear` so the dashboard read path is a
    /// single atomic load.
    ///
    /// Tracks the same fixed-overhead components the old walk attributed
    /// to the client side: the key enum, its Named string length, the
    /// CachedStatement struct, and the `async_name` String capacity for
    /// async clients. The old walk additionally added
    /// `parse.memory_usage()` when `Arc::strong_count(parse) == 1`; that
    /// term is deliberately dropped here because (a) the Arc is the
    /// canonical property of the pool-side `PreparedStatementCache`
    /// (`src/server/prepared_statement_cache.rs::total_memory_bytes`
    /// already accounts for it), so charging it twice when the client is
    /// momentarily the sole owner double-counted; (b) `strong_count`
    /// races with eviction in the pool cache and cannot be maintained as
    /// an incremental delta without coordination across the two caches.
    /// Net effect on `SHOW POOLS_MEMORY` for async clients is a slight
    /// under-report of cache bytes that were never owned exclusively by
    /// the client anyway. For exact accounting use `memory_usage_walk()`.
    total_bytes_approx: AtomicU64,
}

/// Fixed-overhead byte cost of one cache entry. Mirrors the per-entry
/// terms summed by `memory_usage_walk` so the incremental counter
/// converges to the same value the walk produces for the same set of
/// (key, value) pairs (the walk additionally adds the shared `Arc<Parse>`
/// bytes when the client is the sole Arc owner - see the doc on
/// `PreparedStatementCache::total_bytes_approx` for why the incremental
/// counter omits that term).
#[inline]
fn entry_fixed_bytes(key: &PreparedStatementKey, value: &CachedStatement) -> u64 {
    let key_bytes = match key {
        PreparedStatementKey::Named(s) => std::mem::size_of::<PreparedStatementKey>() + s.len(),
        PreparedStatementKey::Anonymous(_) => std::mem::size_of::<PreparedStatementKey>(),
    };
    let stmt_bytes =
        std::mem::size_of::<CachedStatement>() + value.async_name.as_ref().map_or(0, |n| n.len());
    (key_bytes + stmt_bytes) as u64
}

/// Variant of `entry_fixed_bytes` for the Named eviction branch in `put`,
/// which has already moved the evicted `String` key out of `named_order`.
/// Recomputing the key cost from a borrowed `&str` keeps the bookkeeping
/// honest without re-allocating the key.
#[inline]
fn named_entry_fixed_bytes(name_len: usize, value: &CachedStatement) -> u64 {
    std::mem::size_of::<PreparedStatementKey>() as u64 + name_len as u64 + value_only_bytes(value)
}

/// Value-side fixed cost (CachedStatement struct + async_name heap
/// bytes - `Arc<str>` length ). The key cost cancels on
/// Replaced branches where the previous entry and the new entry share
/// the same key allocation (Named) or the same fixed Anonymous overhead.
///
/// the implementation note: switching `async_name` from `String` to `Arc<str>`
/// changes the "owned bytes per entry" accounting from `String::capacity`
/// (which includes spare allocator slack) to `Arc<str>::len` (exact
/// payload + no slack). The walk-vs-approx invariant test
/// (`walk_approx_converge_through_mutations`) compares both sides
/// using the same helper, so the convergence guarantee is preserved.
#[inline]
fn value_only_bytes(value: &CachedStatement) -> u64 {
    std::mem::size_of::<CachedStatement>() as u64
        + value.async_name.as_ref().map_or(0, |n| n.len()) as u64
}

/// Outcome of `PreparedStatementCache::put`.
///
/// `lru::LruCache::push` collapses two distinct cases into the same
/// `Some((k, v))` return: replacement of an existing key, and capacity-driven
/// eviction of a different key. Conflating them produced false positives in
/// the eviction counter when steady-state Parse traffic re-Parsed the same
/// anonymous hash. `PutOutcome` keeps the two apart so callers can bump
/// metrics only on real evictions.
pub enum PutOutcome {
    /// Key was not present; new entry inserted, no value displaced.
    Inserted,
    /// Key was already present; the old value is returned and the entry
    /// remains in the cache. Not an eviction — operator-visible counters
    /// must not increment on this outcome. The displaced value is exposed
    /// for callers that want to observe or drop it explicitly.
    #[allow(dead_code)]
    Replaced(CachedStatement),
    /// Cache was at capacity and a different key was evicted to make room.
    /// Only this outcome should bump the eviction counter. The evicted
    /// value is exposed for callers that want to inspect it before drop.
    #[allow(dead_code)]
    Evicted(CachedStatement),
    /// Named eviction. Carries the evicted client-given name
    /// AND the entry (which exposes `.server_name()` so the caller can
    /// schedule a backend `Close S DOORMAN_N`). Without sending Close
    /// the backend's per-server prepared cache holds the orphan until
    /// its own LRU evicts - leaking PG backend memory under sustained
    /// ORM traffic.
    #[allow(dead_code)]
    NamedEvicted {
        client_name: String,
        entry: CachedStatement,
    },
}

impl std::fmt::Debug for PutOutcome {
    // Variant name is enough for diagnostics; CachedStatement carries an
    // Arc<Parse> with no Debug impl and no useful debug content for tests.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PutOutcome::Inserted => f.write_str("Inserted"),
            PutOutcome::Replaced(_) => f.write_str("Replaced(_)"),
            PutOutcome::Evicted(_) => f.write_str("Evicted(_)"),
            PutOutcome::NamedEvicted { client_name, .. } => {
                write!(f, "NamedEvicted({client_name:?})")
            }
        }
    }
}

// ahash-backed bounded variant. Keys are 64-bit Parse query
// hashes; std `RandomState` would hash them again with SipHash on every
// Bind. AHash is ~2.5-8× faster on short keys and is already used by
// the `Unlimited` variant above (`AHashMap`), so this keeps both
// variants on the same hasher family.
enum AnonymousCache {
    Unlimited(AHashMap<u64, CachedStatement>),
    Limited(LruCache<u64, CachedStatement, ahash::RandomState>),
}

/// hard cap on Named prepared-statement entries per client.
///
/// The Named map was unbounded - a single misbehaving client
/// (e.g. an ORM that names every Parse uniquely, or a malicious client
/// rotating names) could grow it without limit. At ~200 B per entry
/// (statement name + CachedStatement) and 10k entries per client × 10k
/// clients that's ~20 GiB of pinned heap inside the pooler. Easy
/// availability DoS without authentication beyond the initial login.
///
/// Default cap is generous enough for legitimate ORMs (Hibernate, ATS,
/// pgjdbc with batch reuse rarely exceed a few hundred unique prepared
/// statements per session). Operators with extraordinary workloads can
/// raise via the existing `client_anon_cache_size` knob is for Anonymous;
/// Named uses this constant as a hard ceiling and triggers LRU eviction
/// once reached.
pub const MAX_NAMED_PREPARED_PER_CLIENT: usize = 2048;
const MAX_NAMED_ORDER_QUEUE_PER_CLIENT: usize = MAX_NAMED_PREPARED_PER_CLIENT * 2;
pub const MAX_PORTAL_CLEANUP_ATTRIBUTION_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DISABLED_STATEMENT_CLEANUP_ATTRIBUTION_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DISABLED_STATEMENT_CLEANUP_ATTRIBUTION_ENTRIES: usize = MAX_NAMED_PREPARED_PER_CLIENT;

impl PreparedStatementCache {
    /// `anon_size = 0` selects an unlimited Anonymous map (no LRU).
    pub fn new(anon_size: usize) -> Self {
        let anonymous = if anon_size > 0 {
            AnonymousCache::Limited(LruCache::with_hasher(
                NonZeroUsize::new(anon_size).unwrap(),
                ahash::RandomState::new(),
            ))
        } else {
            AnonymousCache::Unlimited(AHashMap::new())
        };
        Self {
            named: AHashMap::new(),
            anonymous,
            named_order: std::collections::VecDeque::new(),
            total_bytes_approx: AtomicU64::new(0),
        }
    }

    /// Returns a reference to the value corresponding to the key.
    /// Updates LRU order for Anonymous + Limited.
    #[inline]
    pub fn get(&mut self, key: &PreparedStatementKey) -> Option<&CachedStatement> {
        match key {
            PreparedStatementKey::Named(s) => self.named.get(s),
            PreparedStatementKey::Anonymous(h) => match &mut self.anonymous {
                AnonymousCache::Unlimited(m) => m.get(h),
                AnonymousCache::Limited(l) => l.get(h),
            },
        }
    }

    /// Insert into the routed map and report what happened.
    ///
    /// Named insertion enforces `MAX_NAMED_PREPARED_PER_CLIENT` as a hard cap
    /// to prevent a
    /// per-client unbounded-growth DoS. Eviction is genuine FIFO via the
    /// parallel `named_order` VecDeque (was hash-bucket order
    /// from `AHashMap::keys().next()`, which could evict the freshly
    /// inserted hot statement). Returns `NamedEvicted { client_name,
    /// entry }` so the caller can:
    ///   (a) bump a Named-specific eviction counter (not Anonymous),
    ///   (b) schedule a backend `Close S DOORMAN_N` so the PG-side
    ///       prepared cache doesn't leak the orphan name.
    /// Re-insert of an existing key returns `Replaced` (no eviction).
    #[must_use = "check for PutOutcome::Evicted / NamedEvicted to bump eviction metrics and send backend Close; otherwise discard with `let _ =`"]
    #[inline]
    pub fn put(&mut self, key: PreparedStatementKey, value: CachedStatement) -> PutOutcome {
        // pre-compute the new entry's fixed-overhead bytes; key and
        // value parts are tracked separately so Replaced branches (where
        // the key allocation persists) can cancel the key term cleanly.
        let new_value_bytes = value_only_bytes(&value);
        let new_full_bytes = entry_fixed_bytes(&key, &value);
        match key {
            PreparedStatementKey::Named(s) => {
                // If the key already exists, plain insert (replacement) -
                // no capacity check needed and no order-queue change
                // (the entry keeps its original position in the FIFO).
                if self.named.contains_key(&s) {
                    let prev = self
                        .named
                        .insert(s, value)
                        .expect("just checked contains_key");
                    // Key allocation persists across the replacement, so
                    // only the value-side delta moves the counter.
                    self.adjust_bytes(new_value_bytes, value_only_bytes(&prev));
                    return PutOutcome::Replaced(prev);
                }
                // Fresh key: if at cap, evict the genuinely-oldest entry
                // via the parallel insertion-order queue.
                if self.named.len() >= MAX_NAMED_PREPARED_PER_CLIENT {
                    if self.named_order.len() != self.named.len() {
                        self.compact_named_order();
                    }
                    // Pop names until we find one still in the map
                    // (defence against drift if a manual `pop` removed
                    // an entry without updating named_order; can also
                    // happen during normal Close handling).
                    while let Some(evict_name) = self.named_order.pop_front() {
                        if let Some(evicted) = self.named.remove(&evict_name) {
                            let evicted_bytes = named_entry_fixed_bytes(evict_name.len(), &evicted);
                            // Now record the new entry and insert.
                            self.named_order.push_back(s.clone());
                            self.named.insert(s, value);
                            self.adjust_bytes(new_full_bytes, evicted_bytes);
                            return PutOutcome::NamedEvicted {
                                client_name: evict_name,
                                entry: evicted,
                            };
                        }
                    }
                    // named_order drained but map said we were at cap -
                    // should not happen in steady state; fall through
                    // to plain insert.
                }
                self.named_order.push_back(s.clone());
                match self.named.insert(s, value) {
                    None => {
                        self.total_bytes_approx
                            .fetch_add(new_full_bytes, Ordering::Relaxed);
                        PutOutcome::Inserted
                    }
                    Some(prev) => {
                        // Defensive: contains_key was false above but
                        // insert returned a value. Treat as replacement
                        // (key allocation persists; value-only delta).
                        self.adjust_bytes(new_value_bytes, value_only_bytes(&prev));
                        PutOutcome::Replaced(prev)
                    }
                }
            }
            PreparedStatementKey::Anonymous(h) => match &mut self.anonymous {
                AnonymousCache::Unlimited(m) => match m.insert(h, value) {
                    None => {
                        self.total_bytes_approx
                            .fetch_add(new_full_bytes, Ordering::Relaxed);
                        PutOutcome::Inserted
                    }
                    Some(prev) => {
                        // Anonymous key is fixed-size and stored inline;
                        // replacement cancels the key term identically.
                        self.adjust_bytes(new_value_bytes, value_only_bytes(&prev));
                        PutOutcome::Replaced(prev)
                    }
                },
                // `LruCache::push` returns `Some((k, v))` for both replacement
                // (key already present, old value returned) and eviction
                // (cache at capacity, oldest entry popped). Disambiguate by
                // probing capacity + presence beforehand so callers can tell
                // a real eviction from a steady-state replacement.
                AnonymousCache::Limited(l) => {
                    let was_at_capacity = l.len() == l.cap().get();
                    let key_existed = l.contains(&h);
                    match l.push(h, value) {
                        None => {
                            self.total_bytes_approx
                                .fetch_add(new_full_bytes, Ordering::Relaxed);
                            PutOutcome::Inserted
                        }
                        Some((_, prev)) if key_existed => {
                            self.adjust_bytes(new_value_bytes, value_only_bytes(&prev));
                            PutOutcome::Replaced(prev)
                        }
                        Some((_, evicted)) => {
                            debug_assert!(
                                was_at_capacity,
                                "LruCache::push returned Some without replacement \
                                 despite cache below capacity",
                            );
                            // Eviction: a different anonymous entry was
                            // bumped out. Anonymous key cost is fixed, so
                            // the evicted entry's bytes mirror the new
                            // entry's structure (key overhead + value).
                            let evicted_bytes = std::mem::size_of::<PreparedStatementKey>() as u64
                                + value_only_bytes(&evicted);
                            self.adjust_bytes(new_full_bytes, evicted_bytes);
                            PutOutcome::Evicted(evicted)
                        }
                    }
                }
            },
        }
    }

    /// Incremental adjustment helper. `add`/`sub` are pre-computed
    /// fixed-overhead deltas; using a single helper keeps the wraparound
    /// guard in one place. Relaxed ordering matches the pool-side
    /// `total_memory_bytes` mirror - readers tolerate transient drift
    /// while concurrent put/pop interleave.
    #[inline]
    fn adjust_bytes(&self, add: u64, sub: u64) {
        if add >= sub {
            self.total_bytes_approx
                .fetch_add(add - sub, Ordering::Relaxed);
        } else {
            self.total_bytes_approx
                .fetch_sub(sub - add, Ordering::Relaxed);
        }
    }

    fn compact_named_order(&mut self) {
        if self.named.is_empty() {
            self.named_order.clear();
            return;
        }

        let mut seen = AHashSet::with_capacity(self.named.len());
        let mut compacted = Vec::with_capacity(self.named.len());
        for name in self.named_order.iter().rev() {
            if self.named.contains_key(name) && seen.insert(name.clone()) {
                compacted.push(name.clone());
            }
        }
        compacted.reverse();
        self.named_order = compacted.into_iter().collect();
    }

    #[inline]
    fn compact_named_order_if_over_budget(&mut self) {
        if self.named_order.len() > MAX_NAMED_ORDER_QUEUE_PER_CLIENT {
            self.compact_named_order();
        }
    }

    /// Removes a key from the cache, returning the value if it existed.
    ///
    /// for Named entries `named_order` is pruned lazily by the
    /// cap-triggered eviction loop in `put` and by the fixed queue budget here.
    /// Eager removal here alters batch-parse-describe behaviour; the
    /// named_order tracking is load-bearing for ParseComplete reordering in
    /// async client mode.
    /// Memory growth of `named_order` is bounded by 2x the cap (4096)
    /// per client; ~150 KiB worst-case per client, acceptable.
    #[inline]
    pub fn pop(&mut self, key: &PreparedStatementKey) -> Option<CachedStatement> {
        let removed = match key {
            PreparedStatementKey::Named(s) => self.named.remove(s),
            PreparedStatementKey::Anonymous(h) => match &mut self.anonymous {
                AnonymousCache::Unlimited(m) => m.remove(h),
                AnonymousCache::Limited(l) => l.pop(h),
            },
        };
        if let Some(ref value) = removed {
            let bytes = entry_fixed_bytes(key, value);
            self.total_bytes_approx.fetch_sub(bytes, Ordering::Relaxed);
        }
        if matches!(key, PreparedStatementKey::Named(_)) {
            self.compact_named_order_if_over_budget();
        }
        removed
    }

    pub fn pop_by_server_name(
        &mut self,
        server_name: &str,
    ) -> Option<(PreparedStatementKey, CachedStatement)> {
        let named_key = self
            .named
            .iter()
            .find_map(|(name, cached)| (cached.server_name() == server_name).then(|| name.clone()));
        if let Some(name) = named_key {
            let key = PreparedStatementKey::Named(name);
            return self.pop(&key).map(|cached| (key, cached));
        }

        let anonymous_key = AnonIter::new(&self.anonymous)
            .find_map(|(hash, cached)| (cached.server_name() == server_name).then_some(hash));
        if let Some(hash) = anonymous_key {
            let key = PreparedStatementKey::Anonymous(hash);
            return self.pop(&key).map(|cached| (key, cached));
        }

        None
    }

    /// Total number of entries across Named and Anonymous maps.
    #[inline]
    pub fn len(&self) -> usize {
        self.named_count() + self.anonymous_count()
    }

    #[allow(dead_code)]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.anonymous_count() == 0
    }

    #[inline]
    pub fn named_count(&self) -> usize {
        self.named.len()
    }

    #[inline]
    pub fn anonymous_count(&self) -> usize {
        match &self.anonymous {
            AnonymousCache::Unlimited(m) => m.len(),
            AnonymousCache::Limited(l) => l.len(),
        }
    }

    /// Clears both Named and Anonymous maps.
    #[inline]
    pub fn clear(&mut self) {
        self.named.clear();
        // keep named_order in sync with the named map.
        self.named_order.clear();
        match &mut self.anonymous {
            AnonymousCache::Unlimited(m) => m.clear(),
            AnonymousCache::Limited(l) => l.clear(),
        }
        // reset the incremental counter together with the maps so
        // the gauge does not lag the truth after DISCARD ALL / DEALLOCATE
        // ALL / disconnect.
        self.total_bytes_approx.store(0, Ordering::Relaxed);
    }

    /// O(1) approximate memory usage of the per-client
    /// prepared cache. Read by `update_prepared_cache_stats` on every
    /// Parse - replaces a per-entry walk that was the dominant cost on
    /// 10k Parse/sec workloads.
    ///
    /// "Approximate" is honest: this counter omits the
    /// shared-`Arc<Parse>` bytes that the previous walk attributed to
    /// the client when `Arc::strong_count == 1`. See the field doc on
    /// `total_bytes_approx` for the rationale. For exact accounting use
    /// `memory_usage_walk()` (intended for tests, admin debug, and
    /// `prepared_cache_memory_benchmarks`).
    #[inline]
    pub fn memory_usage_approx(&self) -> u64 {
        self.total_bytes_approx.load(Ordering::Relaxed)
    }

    /// exact O(N) walk preserved for tests and benchmarks that
    /// need to confirm the incremental counter (`memory_usage_approx`)
    /// converges to the same value the walk produces for any given
    /// snapshot of (key, value) pairs. Not called on the hot path.
    ///
    /// Unlike the original `PreparedStatementState::cache_memory_usage`,
    /// this walk omits the `Arc::strong_count == 1` term so that the
    /// `walk ≈ approx` invariant is checkable without coordinating Arc
    /// ownership with the pool cache. The omitted term is small and
    /// transient (only async clients that hold a unique Arc).
    #[allow(dead_code)] // used by `benches/prepared_cache_memory_benchmarks.rs`
    pub fn memory_usage_walk(&self) -> u64 {
        let mut total: u64 = 0;
        for (key, cached) in self.iter() {
            total += match key {
                PreparedStatementKeyRef::Named(s) => {
                    (std::mem::size_of::<PreparedStatementKey>() + s.len()) as u64
                }
                PreparedStatementKeyRef::Anonymous(_) => {
                    std::mem::size_of::<PreparedStatementKey>() as u64
                }
            };
            total += std::mem::size_of::<CachedStatement>() as u64;
            if let Some(ref name) = cached.async_name {
                // `Arc<str>::len()` replaces `String::capacity()` -
                // see `value_only_bytes` doc for why this still keeps
                // walk and approx in lock-step.
                total += name.len() as u64;
            }
        }
        total
    }

    /// Yields `(borrowed key, value)` for both maps. The Anonymous side
    /// produces `PreparedStatementKeyRef::Anonymous(hash)` keys, the Named
    /// side `PreparedStatementKeyRef::Named(&str)` borrowing the map's key.
    /// Order is unspecified. Note: does not affect LRU order for Anonymous + Limited.
    ///
    /// Returning a borrowed-key view avoids two allocation costs that the
    /// previous `Box<dyn Iterator<Item = (PreparedStatementKey, ...)>>`
    /// signature paid on every call: the trait-object box and a `String`
    /// clone per Named entry.
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (PreparedStatementKeyRef<'_>, &CachedStatement)> + '_ {
        let named_iter = self
            .named
            .iter()
            .map(|(k, v)| (PreparedStatementKeyRef::Named(k.as_str()), v));
        let anon_iter =
            AnonIter::new(&self.anonymous).map(|(h, v)| (PreparedStatementKeyRef::Anonymous(h), v));
        named_iter.chain(anon_iter)
    }
}

/// Unifies the two backing iterator types of `AnonymousCache`
/// (`std::collections::hash_map::Iter` and `lru::Iter`) into a single
/// concrete type so `PreparedStatementCache::iter` can return
/// `impl Iterator` without boxing. `AHashMap` derefs to `std::HashMap`,
/// so its `iter()` returns the standard library's hash_map iterator.
enum AnonIter<'a> {
    Unlimited(std::collections::hash_map::Iter<'a, u64, CachedStatement>),
    Limited(lru::Iter<'a, u64, CachedStatement>),
}

impl<'a> AnonIter<'a> {
    fn new(anon: &'a AnonymousCache) -> Self {
        match anon {
            AnonymousCache::Unlimited(m) => AnonIter::Unlimited(m.iter()),
            AnonymousCache::Limited(l) => AnonIter::Limited(l.iter()),
        }
    }
}

impl<'a> Iterator for AnonIter<'a> {
    type Item = (u64, &'a CachedStatement);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            AnonIter::Unlimited(it) => it.next().map(|(h, v)| (*h, v)),
            AnonIter::Limited(it) => it.next().map(|(h, v)| (*h, v)),
        }
    }
}

/// What response message we're waiting for to insert ParseComplete
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseCompleteTarget {
    /// Waiting for BindComplete - insert ParseComplete before it
    BindComplete,
    /// Waiting for ParameterDescription - insert ParseComplete before it (Describe flow)
    ParameterDescription,
}

/// Tracks a skipped Parse message that needs a synthetic ParseComplete response
#[derive(Debug, Clone)]
pub struct SkippedParse {
    /// The rewritten statement name (e.g., DOORMAN_5).
    /// `Arc<str>` because every Bind/Describe/Close that
    /// references this skipped Parse used to clone the name as `String`;
    /// the refcount bump is ~12× cheaper than a per-message allocation
    /// on the extended-protocol hot path.
    pub statement_name: Arc<str>,
    /// What response we're waiting for to insert ParseComplete
    pub target: ParseCompleteTarget,
    /// If true, ParseComplete should be inserted at the beginning of the response.
    /// This is set when a skipped Parse comes before a new Parse in the same batch,
    /// AND there is no corresponding Bind for this skipped Parse yet.
    pub insert_at_beginning: bool,
    /// If true, a Bind message for this statement has been processed.
    /// This prevents marking insert_at_beginning=true when a new Parse arrives,
    /// because the ParseComplete should be inserted before BindComplete, not at beginning.
    pub has_bind: bool,
}

/// Tracks response message counts across multiple chunks.
/// Replaces HashMap<char, usize> with fixed fields for better performance.
#[derive(Debug, Clone, Default)]
pub struct ResponseCounts {
    /// Count of ParseComplete ('1') messages
    pub parse_complete: usize,
    /// Count of BindComplete ('2') messages
    pub bind_complete: usize,
    /// Count of ParameterDescription ('t') messages
    pub param_desc: usize,
    /// Count of Describe Portal RowDescription/NoData ('T'/'n') messages
    pub portal_desc: usize,
    /// Number of statement Describe RowDescription/NoData frames still expected
    /// after already-seen ParameterDescription frames.
    pub statement_desc_pending: usize,
    /// Count of Execute (tracked via CommandComplete 'C') messages
    pub execute: usize,
    /// Count of CloseComplete ('3') messages
    pub close_complete: usize,
}

impl ResponseCounts {
    #[inline(always)]
    pub fn clear(&mut self) {
        self.parse_complete = 0;
        self.bind_complete = 0;
        self.param_desc = 0;
        self.portal_desc = 0;
        self.statement_desc_pending = 0;
        self.execute = 0;
        self.close_complete = 0;
    }
}

/// Tracks operations in a batch to determine correct ParseComplete insertion order
///
/// `statement_name: Arc<str>` because every extended-protocol
/// roundtrip pushes multiple entries that reference the same name (Parse,
/// Bind, Describe, Close). Cloning the name as `String` per entry was a
/// per-message allocation; `Arc<str>::clone` is a refcount bump.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum BatchOperation {
    /// Parse was skipped (statement already on server)
    ParseSkipped { statement_name: Arc<str> },
    /// Parse was sent to server
    ParseSent { statement_name: Arc<str> },
    /// Describe statement (produces ParameterDescription + RowDescription)
    Describe { statement_name: Arc<str> },
    /// Describe portal (produces RowDescription only)
    DescribePortal,
    /// Bind to statement
    Bind { statement_name: Arc<str> },
    /// Execute portal (produces DataRow + CommandComplete)
    Execute,
    /// Close statement or portal (produces CloseComplete)
    Close,
}

/// Cached prepared statement entry.
/// For async clients, stores an optional unique name to avoid "prepared statement already exists" errors.
#[derive(Clone)]
pub struct CachedStatement {
    /// Shared Parse from pool cache (contains query text)
    pub parse: Arc<Parse>,
    /// Hash of the statement
    pub hash: u64,
    /// True when this cache entry came from extended-protocol `DISCARD ALL`
    /// interception and its backend Parse was rewritten to a no-op.
    pub intercepted_discard_all: bool,
    /// Cleanup attribution for successful extended-protocol executions of this
    /// statement. PostgreSQL reports ordinary SET, SET ROLE, and
    /// SET SESSION AUTHORIZATION with the same CommandComplete("SET") tag, so
    /// the Execute path records this metadata before relaying the response.
    pub(crate) set_cleanup_command: Option<SetCleanupCommand>,
    /// Cleanup attribution for successful extended-protocol RESET executions.
    pub(crate) reset_cleanup_command: Option<ResetCleanupCommand>,
    /// Unique statement name for async clients (e.g., "DOORMAN_async_12345").
    /// None for non-async clients (they use `parse.name` directly).
    ///
    /// stored as `Arc<str>` because every Bind/Describe/
    /// Close on the extended-protocol hot path used to clone the name as
    /// `String`. The refcount-bump path lets the per-batch
    /// `BatchOperation`/`SkippedParse` entries share the same allocation
    /// the cache itself already owns.
    pub async_name: Option<Arc<str>>,
}

impl CachedStatement {
    /// Build a plain cached statement entry with no cleanup attribution.
    #[must_use]
    pub fn new(parse: Arc<Parse>, hash: u64, async_name: Option<Arc<str>>) -> Self {
        Self {
            parse,
            hash,
            intercepted_discard_all: false,
            set_cleanup_command: None,
            reset_cleanup_command: None,
            async_name,
        }
    }

    /// Returns the statement name to use when communicating with the server.
    /// For async clients, returns the unique async_name; otherwise returns parse.name.
    #[inline(always)]
    pub fn server_name(&self) -> &str {
        self.async_name
            .as_ref()
            .map(|s| s.as_ref())
            .unwrap_or(&self.parse.name)
    }

    /// owned `Arc<str>` view over the server-side
    /// statement name. For async clients this is a cheap refcount bump
    /// on the already-`Arc<str>` async name. For non-async clients
    /// `parse.name` is still a `String`, so this path allocates once
    /// per call - removing it needs the deferred `Parse.name
    /// -> Arc<str>` migration, a separate change.
    #[inline]
    pub fn server_name_arc(&self) -> Arc<str> {
        match &self.async_name {
            Some(a) => Arc::clone(a),
            None => Arc::<str>::from(self.parse.name.as_str()),
        }
    }
}

/// State related to prepared statements handling.
/// Groups all fields needed for prepared statement caching and batch processing.
pub struct PreparedStatementState {
    /// Whether prepared statements are enabled for this client
    pub enabled: bool,

    /// Whether this client has ever used async protocol (Flush command)
    /// Once set to true, prepared statements caching is disabled for this client
    pub async_client: bool,

    /// Mapping of client named prepared statement to cached statement info
    pub cache: PreparedStatementCache,

    /// Hash of the last anonymous prepared statement (for Bind to find the corresponding Parse)
    pub last_anonymous_hash: Option<u64>,

    /// Hash of the last Bind in the current batch, plus the anonymous flag.
    /// Cleared on Sync completion. Used by /api/top/queries duration
    /// instrumentation to attribute the batch's elapsed time to a single
    /// interner entry.
    pub last_bound_for_top: Option<(u64, bool)>,

    /// Tracks skipped Parse messages that need synthetic ParseComplete responses.
    /// Each entry contains the statement name and what response we're waiting for.
    pub skipped_parses: Vec<SkippedParse>,

    /// Tracks all operations in current batch to determine correct ParseComplete insertion order.
    /// Cleared after Sync.
    pub batch_operations: Vec<BatchOperation>,

    /// Cleanup attribution copied from prepared statement to bound portal.
    /// Extended Parse does not execute SQL; Bind creates the portal and Execute
    /// is the point where a later CommandComplete must be attributed.
    pub portal_set_cleanup_commands: AHashMap<String, SetCleanupCommand>,
    pub portal_reset_cleanup_commands: AHashMap<String, ResetCleanupCommand>,
    pub portal_cleanup_attribution_bytes: usize,
    /// When client-side prepared statement caching is disabled, Parse frames
    /// still define statement names whose cleanup attribution must be copied
    /// to portals on Bind and applied on Execute.
    pub disabled_statement_set_cleanup_commands: AHashMap<String, SetCleanupCommand>,
    pub disabled_statement_reset_cleanup_commands: AHashMap<String, ResetCleanupCommand>,
    pub disabled_statement_cleanup_attribution_bytes: usize,

    /// Counter for Parse messages sent to server in current batch.
    /// Used to determine if skipped Parse should insert ParseComplete at beginning or before BindComplete.
    pub parses_sent_in_batch: u32,

    /// Tracks how many BindComplete/ParameterDescription messages have been processed
    /// across multiple response chunks. Used for correct ParseComplete insertion.
    pub processed_response_counts: ResponseCounts,

    /// Counter for pending CloseComplete messages to send before ReadyForQuery
    pub pending_close_complete: u32,

    /// Cumulative count of Anonymous LRU evictions in this client's cache.
    /// Surfaced via the `pg_doorman_clients_prepared_anonymous_evictions_total`
    /// Prometheus counter; a sustained non-zero rate signals that
    /// `client_anonymous_prepared_cache_size` is too small for the workload.
    pub anonymous_evictions: u64,
    /// cumulative count of Named LRU evictions due to
    /// `MAX_NAMED_PREPARED_PER_CLIENT` cap pressure. Distinct from
    /// `anonymous_evictions`; earlier both bucketed together under
    /// the Anonymous metric, hiding genuine ORM working-set pressure.
    pub named_evictions: u64,
}

impl PreparedStatementState {
    /// Create a new PreparedStatementState. `anon_cache_size = 0` selects an
    /// unlimited Anonymous map (no LRU eviction); the Named map is always
    /// unbounded.
    pub fn new(enabled: bool, anon_cache_size: usize) -> Self {
        Self {
            enabled,
            async_client: false,
            cache: PreparedStatementCache::new(anon_cache_size),
            last_anonymous_hash: None,
            last_bound_for_top: None,
            skipped_parses: Vec::new(),
            batch_operations: Vec::new(),
            portal_set_cleanup_commands: AHashMap::new(),
            portal_reset_cleanup_commands: AHashMap::new(),
            portal_cleanup_attribution_bytes: 0,
            disabled_statement_set_cleanup_commands: AHashMap::new(),
            disabled_statement_reset_cleanup_commands: AHashMap::new(),
            disabled_statement_cleanup_attribution_bytes: 0,
            parses_sent_in_batch: 0,
            processed_response_counts: ResponseCounts::default(),
            pending_close_complete: 0,
            anonymous_evictions: 0,
            named_evictions: 0,
        }
    }

    /// Reset batch state after Sync
    #[inline(always)]
    pub fn reset_batch(&mut self) {
        self.parses_sent_in_batch = 0;
        self.skipped_parses.clear();
        self.batch_operations.clear();
        self.processed_response_counts.clear();
    }

    #[inline(always)]
    pub fn clear_portal_cleanup_commands(&mut self) {
        self.portal_set_cleanup_commands.clear();
        self.portal_reset_cleanup_commands.clear();
        self.portal_cleanup_attribution_bytes = 0;
    }

    #[inline(always)]
    fn portal_cleanup_entry_bytes(portal_name: &str) -> usize {
        std::mem::size_of::<String>()
            .saturating_add(portal_name.len())
            .saturating_add(
                std::mem::size_of::<SetCleanupCommand>()
                    .max(std::mem::size_of::<ResetCleanupCommand>()),
            )
    }

    #[inline]
    fn reserve_portal_cleanup_attribution(&mut self, portal_name: &str) -> Result<(), Error> {
        let entry_bytes = Self::portal_cleanup_entry_bytes(portal_name);
        if self
            .portal_cleanup_attribution_bytes
            .saturating_add(entry_bytes)
            > MAX_PORTAL_CLEANUP_ATTRIBUTION_BYTES
        {
            return Err(Error::ClientError(format!(
                "extended-protocol portal cleanup attribution would exceed \
                 {MAX_PORTAL_CLEANUP_ATTRIBUTION_BYTES} bytes"
            )));
        }
        self.portal_cleanup_attribution_bytes = self
            .portal_cleanup_attribution_bytes
            .saturating_add(entry_bytes);
        Ok(())
    }

    #[inline]
    fn remove_portal_set_cleanup_command(&mut self, portal_name: &str) {
        if self
            .portal_set_cleanup_commands
            .remove(portal_name)
            .is_some()
        {
            self.portal_cleanup_attribution_bytes = self
                .portal_cleanup_attribution_bytes
                .saturating_sub(Self::portal_cleanup_entry_bytes(portal_name));
        }
    }

    #[inline]
    fn remove_portal_reset_cleanup_command(&mut self, portal_name: &str) {
        if self
            .portal_reset_cleanup_commands
            .remove(portal_name)
            .is_some()
        {
            self.portal_cleanup_attribution_bytes = self
                .portal_cleanup_attribution_bytes
                .saturating_sub(Self::portal_cleanup_entry_bytes(portal_name));
        }
    }

    #[inline]
    pub fn track_portal_set_cleanup_command(
        &mut self,
        portal_name: &str,
        command: SetCleanupCommand,
    ) -> Result<(), Error> {
        self.remove_portal_reset_cleanup_command(portal_name);
        if !self.portal_set_cleanup_commands.contains_key(portal_name) {
            self.reserve_portal_cleanup_attribution(portal_name)?;
        }
        self.portal_set_cleanup_commands
            .insert(portal_name.to_string(), command);
        Ok(())
    }

    #[inline]
    pub fn track_portal_reset_cleanup_command(
        &mut self,
        portal_name: &str,
        command: ResetCleanupCommand,
    ) -> Result<(), Error> {
        self.remove_portal_set_cleanup_command(portal_name);
        if !self.portal_reset_cleanup_commands.contains_key(portal_name) {
            self.reserve_portal_cleanup_attribution(portal_name)?;
        }
        self.portal_reset_cleanup_commands
            .insert(portal_name.to_string(), command);
        Ok(())
    }

    #[inline]
    pub fn remove_portal_cleanup_command(&mut self, portal_name: &str) {
        self.remove_portal_set_cleanup_command(portal_name);
        self.remove_portal_reset_cleanup_command(portal_name);
    }

    #[inline(always)]
    pub fn clear_disabled_statement_cleanup_commands(&mut self) {
        self.disabled_statement_set_cleanup_commands.clear();
        self.disabled_statement_reset_cleanup_commands.clear();
        self.disabled_statement_cleanup_attribution_bytes = 0;
    }

    #[inline(always)]
    fn disabled_statement_cleanup_entry_bytes(statement_name: &str) -> usize {
        Self::portal_cleanup_entry_bytes(statement_name)
    }

    #[inline]
    fn reserve_disabled_statement_cleanup_attribution(
        &mut self,
        statement_name: &str,
    ) -> Result<(), Error> {
        let entries = self
            .disabled_statement_set_cleanup_commands
            .len()
            .saturating_add(self.disabled_statement_reset_cleanup_commands.len());
        if entries >= MAX_DISABLED_STATEMENT_CLEANUP_ATTRIBUTION_ENTRIES {
            return Err(Error::ClientError(format!(
                "extended-protocol disabled-statement cleanup attribution would exceed \
                 {MAX_DISABLED_STATEMENT_CLEANUP_ATTRIBUTION_ENTRIES} entries"
            )));
        }
        let entry_bytes = Self::disabled_statement_cleanup_entry_bytes(statement_name);
        if self
            .disabled_statement_cleanup_attribution_bytes
            .saturating_add(entry_bytes)
            > MAX_DISABLED_STATEMENT_CLEANUP_ATTRIBUTION_BYTES
        {
            return Err(Error::ClientError(format!(
                "extended-protocol disabled-statement cleanup attribution would exceed \
                 {MAX_DISABLED_STATEMENT_CLEANUP_ATTRIBUTION_BYTES} bytes"
            )));
        }
        self.disabled_statement_cleanup_attribution_bytes = self
            .disabled_statement_cleanup_attribution_bytes
            .saturating_add(entry_bytes);
        Ok(())
    }

    #[inline]
    fn remove_disabled_statement_set_cleanup_command(&mut self, statement_name: &str) {
        if self
            .disabled_statement_set_cleanup_commands
            .remove(statement_name)
            .is_some()
        {
            self.disabled_statement_cleanup_attribution_bytes = self
                .disabled_statement_cleanup_attribution_bytes
                .saturating_sub(Self::disabled_statement_cleanup_entry_bytes(statement_name));
        }
    }

    #[inline]
    fn remove_disabled_statement_reset_cleanup_command(&mut self, statement_name: &str) {
        if self
            .disabled_statement_reset_cleanup_commands
            .remove(statement_name)
            .is_some()
        {
            self.disabled_statement_cleanup_attribution_bytes = self
                .disabled_statement_cleanup_attribution_bytes
                .saturating_sub(Self::disabled_statement_cleanup_entry_bytes(statement_name));
        }
    }

    #[inline]
    pub fn track_disabled_statement_set_cleanup_command(
        &mut self,
        statement_name: String,
        command: SetCleanupCommand,
    ) -> Result<(), Error> {
        self.remove_disabled_statement_reset_cleanup_command(statement_name.as_str());
        if !self
            .disabled_statement_set_cleanup_commands
            .contains_key(statement_name.as_str())
        {
            self.reserve_disabled_statement_cleanup_attribution(statement_name.as_str())?;
        }
        self.disabled_statement_set_cleanup_commands
            .insert(statement_name, command);
        Ok(())
    }

    #[inline]
    pub fn track_disabled_statement_reset_cleanup_command(
        &mut self,
        statement_name: String,
        command: ResetCleanupCommand,
    ) -> Result<(), Error> {
        self.remove_disabled_statement_set_cleanup_command(statement_name.as_str());
        if !self
            .disabled_statement_reset_cleanup_commands
            .contains_key(statement_name.as_str())
        {
            self.reserve_disabled_statement_cleanup_attribution(statement_name.as_str())?;
        }
        self.disabled_statement_reset_cleanup_commands
            .insert(statement_name, command);
        Ok(())
    }

    #[inline]
    pub fn remove_disabled_statement_cleanup_command(&mut self, statement_name: &str) {
        self.remove_disabled_statement_set_cleanup_command(statement_name);
        self.remove_disabled_statement_reset_cleanup_command(statement_name);
    }

    /// Drop all client-side prepared-statement state. Used by both the
    /// synthetic-`DISCARD ALL` fast path and the explicit `DEALLOCATE ALL`
    /// handler - both promise the client that the next `Bind` will not find
    /// any named or anonymous prepared statements.
    ///
    /// Returns the number of named/anonymous entries that were cleared, for
    /// caller-side logging.
    ///
    /// Extracted into a helper so the two callers cannot drift out of sync:
    /// missing one field on one of the paths would let a stale hash dedupe
    /// the next anonymous `Parse` against an evicted entry and crash the
    /// client with SQLSTATE 26000 (`prepared statement <name> does not exist`).
    /// Caller is expected to follow up with `update_prepared_cache_stats()`
    /// so the per-client memory / count counters surfaced by SHOW POOLS and
    /// Prometheus reflect the now-empty cache.
    pub fn discard_clear(&mut self) -> usize {
        let cleared = self.cache.len();
        self.cache.clear();
        // Extended-protocol scratch that was attributed to statements we
        // just forgot about. Leaving these populated would let stale Parse
        // hashes leak into the next Sync attribution and confuse the
        // interner.
        self.skipped_parses.clear();
        self.batch_operations.clear();
        self.clear_portal_cleanup_commands();
        self.clear_disabled_statement_cleanup_commands();
        self.parses_sent_in_batch = 0;
        self.last_bound_for_top = None;
        // `last_anonymous_hash` points at a hash that has just been evicted
        // from `cache`. Keeping it would make the very next anonymous Parse
        // dedupe against a stale hash and skip the server-side prepare.
        self.last_anonymous_hash = None;
        // Symmetry with `reset_batch` - DISCARD ALL on the simple-query
        // path implies the extended-protocol pipeline is idle, so this is
        // almost always already empty, but resetting here guards against
        // future batch-pipelining changes that could leave stale per-hash
        // response counts attributed to the cache we just dropped.
        self.processed_response_counts.clear();
        // Symmetry with `reset_buffered_state` (which also resets this
        // counter alongside `skipped_parses` / `parses_sent_in_batch`).
        // In normal flow `pending_close_complete` is 0 when a simple-query
        // DISCARD ALL lands (extended-protocol `Close` requires a Sync
        // that would have drained it), but a future driver that pipelines
        // `Close S 'foo'` -> `Sync` -> `DISCARD ALL` would otherwise leave
        // a stale CloseComplete to be injected into the next batch by
        // the reordering logic in `protocol.rs`.
        self.pending_close_complete = 0;
        cleared
    }

    /// Returns the number of Named entries in the cache.
    /// Used by SHOW POOLS_MEMORY and Prometheus to break down per-client cache.
    #[inline(always)]
    pub fn named_count(&self) -> usize {
        self.cache.named_count()
    }

    /// Returns the number of Anonymous entries in the cache.
    /// Used by SHOW POOLS_MEMORY and Prometheus to break down per-client cache.
    #[inline(always)]
    pub fn anonymous_count(&self) -> usize {
        self.cache.anonymous_count()
    }

    /// Returns the cumulative count of Anonymous LRU evictions in this cache.
    #[inline(always)]
    pub fn anonymous_evictions(&self) -> u64 {
        self.anonymous_evictions
    }

    /// O(1) approximate memory usage of the per-client
    /// prepared cache. Delegates to the incremental
    /// `PreparedStatementCache::memory_usage_approx` counter so callers
    /// pay one relaxed atomic load per Parse instead of walking every
    /// entry.
    ///
    /// The previous implementation walked both Named and Anonymous maps
    /// and added `parse.memory_usage()` when the client held the sole
    /// `Arc<Parse>`. The Arc term is now attributed solely to the
    /// pool-side cache (which already mirrors it in
    /// `src/server/prepared_statement_cache.rs::total_memory_bytes`), so
    /// `SHOW POOLS_MEMORY` for async clients under-reports by the size
    /// of any Parse whose Arc the client momentarily owns alone. That
    /// term was always racing with eviction in the pool cache and could
    /// not have been maintained incrementally without cross-cache
    /// coordination. Net effect is a small approximation traded for
    /// removing an O(N) hot path that, at 10k Parse/sec on the default
    /// 8192-entry Anonymous LRU, drove ~80M atomic loads/sec.
    #[inline]
    pub fn cache_memory_usage(&self) -> usize {
        self.cache.memory_usage_approx() as usize
    }
}

impl Default for PreparedStatementState {
    fn default() -> Self {
        Self::new(false, 0) // 0 = unlimited
    }
}

/// The client state. One of these is created per client.
pub struct Client<S, T> {
    /// The reads are buffered (8K by default).
    pub(crate) read: BufReader<S>,

    /// We buffer the writes ourselves because we know the protocol
    /// better than a stock buffer.
    pub(crate) write: T,

    /// Internal buffer, where we place messages until we have to flush
    /// them to the backend.
    pub(crate) buffer: PooledBuffer,

    /// Address
    pub(crate) addr: std::net::SocketAddr,

    /// Cached string representation of addr — avoids per-query allocation in debug logging.
    pub(crate) addr_str: String,

    /// Reusable read buffer. Avoids heap allocation per message — clear()+reserve()
    /// reuses existing capacity. split() returns owned data to callers.
    pub(crate) read_buf: BytesMut,

    /// Monotonic connection ID assigned at TCP accept. Used in log prefix as `#cN`.
    /// Also serves as Cancel Protocol process_id (as `connection_id as i32`).
    pub(crate) connection_id: u64,

    /// The client was started with the sole reason to cancel another running query.
    pub(crate) cancel_mode: bool,

    /// In transaction mode, the connection is released after each transaction.
    /// Session mode has slightly higher throughput per client, but lower capacity.
    pub(crate) transaction_mode: bool,

    /// Transaction-mode backend affinity required after SQL-level PREPARE.
    /// SQL PREPARE lives in PostgreSQL session state and cannot be moved across
    /// backend checkouts the way extended-protocol cache entries can.
    pub(crate) sql_prepare_session_pinned: bool,

    /// For query cancellation, the client is given a random secret on startup.
    pub(crate) secret_key: i32,

    /// Clients are mapped to servers while they use them. This allows a client
    /// to connect and cancel a query.
    pub(crate) client_server_map: ClientServerMap,

    /// Statistics related to this client
    pub(crate) stats: Arc<ClientStats>,

    /// Clients want to talk to admin database.
    pub(crate) admin: bool,

    /// Last server process stats we talked to.
    pub(crate) last_server_stats: Option<Arc<ServerStats>>,

    /// Connected to server
    pub(crate) connected_to_server: bool,

    /// Session mode: transaction start timestamp for per-transaction xact_time.
    /// Set when server transitions into a transaction (ReadyForQuery 'T'/'E').
    /// Consumed when transaction ends (ReadyForQuery 'I').
    pub(crate) session_xact_start: Option<quanta::Instant>,

    /// Name of the server pool for this client (This comes from the database name in the connection string)
    pub(crate) pool_name: String,

    /// Authenticated Postgres user for this client, used for logs and client-visible stats.
    pub(crate) username: String,

    /// cached route pool identifier so per-client
    /// `get_pool()` lookups skip the `String::to_string()` × 2 that the
    /// `get_pool(&db, &user)` helper paid on every checkout.
    /// Usually `{ db: pool_name, user: username }`; auth_query dedicated
    /// mode keeps `username` as the authenticated client identity and routes
    /// through auth_query.server_user here.
    pub(crate) cached_pool_id: PoolIdentifier,

    /// Pool generation captured at authenticated-client handle startup.
    /// Migration must serialize identity/backend-auth from this held generation
    /// rather than re-reading live global POOLS after a reload.
    pub(crate) migration_pool: Option<ConnectionPool>,

    /// Whether `migration_pool` was dynamic when captured. Dynamic auth_query
    /// generations are not safely migratable because their overlay state is not
    /// represented by the static config hash.
    pub(crate) migration_pool_is_dynamic: bool,

    /// Server startup and session parameters tracked for this client.
    /// Also owns the lazy planner-state hash used by prepared-statement
    /// cache keys.
    pub(crate) server_parameters: ServerParameters,

    /// Prepared statements state (caching, batch operations, etc.)
    pub(crate) prepared: PreparedStatementState,

    pub(crate) max_memory_usage: u64,

    pub(crate) client_last_messages_in_tx: PooledBuffer,

    /// Pending BEGIN message for deferred connection optimization.
    /// When client sends standalone "begin;", we synthesize response
    /// and defer actual BEGIN until next query arrives.
    pub(crate) client_pending_begin: Option<BytesMut>,

    /// deferred `SET application_name TO ...` SQL produced by the
    /// checkout-time `SyncPlan::AppNameOnly` classifier. Task 3 will flush it
    /// concatenated with the client's first frame and then drain the extra
    /// response via `Server::swallow_set_response`. Unused in Task 2 (the
    /// AppNameOnly branch still delegates to `sync_parameters`), so it stays
    /// `None` everywhere for now.
    pub(crate) pending_app_name_set: Option<String>,

    /// Raw fd of the client TCP socket. Stored before tokio::io::split()
    /// because ReadHalf/WriteHalf do not expose as_raw_fd().
    /// Used for client migration during graceful reload.
    #[cfg(unix)]
    pub(crate) raw_fd: Option<std::os::unix::io::RawFd>,

    /// Raw pointer to the OpenSSL SSL object for TLS migration export.
    #[cfg(all(unix, feature = "tls-migration"))]
    pub(crate) ssl_ptr: Option<SslRawPtr>,
}

/// Wrapper around *mut c_void that implements Send+Sync.
/// Used to store the SSL* pointer for migration export.
/// SAFETY: the pointer is only used at the idle point in handle() to call
/// SSL_export_migration_state, which reads TLS state without mutation.
/// The Client task is the sole user — no concurrent access.
#[cfg(all(unix, feature = "tls-migration"))]
#[derive(Clone, Copy)]
pub struct SslRawPtr(pub(crate) *mut std::ffi::c_void);
#[cfg(all(unix, feature = "tls-migration"))]
unsafe impl Send for SslRawPtr {}
#[cfg(all(unix, feature = "tls-migration"))]
unsafe impl Sync for SslRawPtr {}

impl<S, T> Client<S, T>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    #[inline(always)]
    pub fn is_admin(&self) -> bool {
        self.admin
    }

    #[inline(always)]
    pub(crate) fn disconnect_stats(&self) {
        self.stats.disconnect();
    }

    /// Updates the prepared cache statistics in ClientStats.
    /// Should be called after any modification to prepared.cache.
    #[inline(always)]
    pub(crate) fn update_prepared_cache_stats(&self) {
        self.stats
            .set_prepared_cache_stats(PreparedCacheSnapshot::new(
                self.prepared.cache_memory_usage() as u64,
                self.prepared.named_count() as u64,
                self.prepared.anonymous_count() as u64,
                self.prepared.anonymous_evictions(),
                // surface Named cap evictions to ClientStats.
                self.prepared.named_evictions,
            ));
    }

    /// Retrieve connection pool, if it exists.
    /// Return an error to the client otherwise.
    pub(crate) async fn get_pool(&mut self) -> Result<ConnectionPool, Error> {
        // lookup via cached PoolIdentifier - no per-checkout
        // String allocation. See `Client.cached_pool_id`.
        let live_pool = get_pool_by_id(&self.cached_pool_id);
        if self.migration_pool_is_dynamic {
            if let Some(captured) = self.migration_pool.as_ref() {
                let live_matches_capture = live_pool
                    .as_ref()
                    .is_some_and(|pool| Arc::ptr_eq(&pool.init_complete, &captured.init_complete));
                if live_matches_capture && !captured.database.is_closed() {
                    return Ok(captured.clone());
                }
            }
            return self.missing_pool_error().await;
        }
        if let Some(pool) = live_pool {
            return Ok(pool);
        }
        self.missing_pool_error().await
    }

    async fn missing_pool_error(&mut self) -> Result<ConnectionPool, Error> {
        let client_msg = format!(
            "No pool configured for database: {}, user: {}",
            self.pool_name, self.username
        );
        let err = Error::ClientError(format!(
            "Invalid pool name {{ username: {}, pool_name: {}, application_name: {} }}",
            self.pool_name,
            self.username,
            self.server_parameters.get_application_name(),
        ));
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        if let Err(write_err) =
            error_response_timeout(&mut self.write, &client_msg, "3D000", write_timeout).await
        {
            log::warn!(
                "[{}@{} #c{}] failed to send missing-pool ErrorResponse: {write_err}",
                self.username,
                self.pool_name,
                self.connection_id,
            );
        }

        Err(err)
    }

    /// Release the server from the client: it can't cancel its queries anymore.
    #[inline(always)]
    pub fn release(&self) {
        self.client_server_map
            .remove(&(self.connection_id as i32, self.secret_key));
    }

    /// Detach a checked-out backend before propagating an inner handler error.
    ///
    /// The backend `Object` may be dropped and returned to the pool before
    /// `Client::Drop` runs, so cancel routing must be removed immediately.
    #[inline(always)]
    pub(crate) fn release_after_inner_handler_error(&mut self) {
        self.connected_to_server = false;
        self.release();
    }
}

#[cfg(test)]
mod no_pool_error_tests {
    #[test]
    fn missing_pool_error_response_is_deadline_bound() {
        let src = include_str!("core.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("pub(crate) async fn get_pool")
            .expect("get_pool helper not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Release the server")
            .expect("release marker should follow get_pool");
        let body = &body[..end];

        assert!(
            body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "missing-pool ErrorResponse must use proxy_copy_data_timeout"
        );
        assert!(
            body.contains("error_response_timeout(") && body.contains("&mut self.write"),
            "missing-pool ErrorResponse must be deadline-bound"
        );
        assert!(
            !body.contains("error_response(\n"),
            "missing-pool ErrorResponse must not use bare write_all_flush"
        );
    }

    #[test]
    fn dynamic_auth_query_clients_reject_captured_pool_after_global_removal() {
        let src = include_str!("core.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let start = impl_src
            .find("pub(crate) async fn get_pool")
            .expect("get_pool helper not found");
        let body = &impl_src[start..];
        let end = body
            .find("\n    /// Release the server")
            .expect("release marker should follow get_pool");
        let body = &body[..end];
        let missing_pool_idx = body
            .find("let client_msg = format!")
            .expect("missing-pool error path must exist");
        let before_error = &body[..missing_pool_idx];

        assert!(
            before_error.contains("self.migration_pool_is_dynamic")
                && before_error.contains("self.migration_pool.as_ref()")
                && before_error.contains("Arc::ptr_eq")
                && before_error.contains(".database.is_closed()"),
            "dynamic auth_query clients must inspect their captured generation \
             before reporting a missing pool"
        );
        assert!(
            !before_error.contains("return Ok(pool.clone())"),
            "dynamic auth_query clients must not keep checking out from a \
             captured generation after reload/refetch removed the id from POOLS"
        );
    }
}

impl<S, T> Drop for Client<S, T> {
    fn drop(&mut self) {
        // a cancel-mode Client overloads
        // (connection_id, secret_key) with the VICTIM's pid+secret (see
        // Client::cancel). It never owns a routing row of its own, so Drop
        // must not remove the victim's live entry - doing so leaves the victim
        // un-cancellable for the rest of its in-flight transaction (every
        // retry cancel then becomes a silent no-op).
        if !self.cancel_mode {
            self.client_server_map
                .remove(&(self.connection_id as i32, self.secret_key));
        }

        // Update server stats if the client was connected to a server
        if self.connected_to_server {
            if let Some(stats) = self.last_server_stats.as_ref() {
                stats.idle(0);
            }
        }

        // Ensure client is removed from stats tracking when dropped
        // This handles cases where client disconnects unexpectedly (e.g., TCP abort)
        self.stats.disconnect();
    }
}

#[cfg(test)]
mod cache_split_tests {
    use super::*;
    use crate::config::tls::{ServerTlsConfig, ServerTlsMode};
    use crate::pool::CancelTarget;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tokio::io::{empty, sink, Empty, Sink};

    fn make_cached(name: &str, query: &str) -> CachedStatement {
        let mut buf = bytes::BytesMut::new();
        use bytes::BufMut;
        buf.put_u8(b'P');
        let name_bytes = name.as_bytes();
        let query_bytes = query.as_bytes();
        let len = 4 + name_bytes.len() + 1 + query_bytes.len() + 1 + 2;
        buf.put_i32(len as i32);
        buf.put_slice(name_bytes);
        buf.put_u8(0);
        buf.put_slice(query_bytes);
        buf.put_u8(0);
        buf.put_i16(0);
        let parse: crate::messages::Parse = (&buf).try_into().unwrap();
        CachedStatement {
            parse: Arc::new(parse),
            hash: 0xdead_beef,
            intercepted_discard_all: false,
            set_cleanup_command: None,
            reset_cleanup_command: None,
            async_name: None,
        }
    }

    fn test_client_with_cancel_map() -> (Client<Empty, Sink>, ClientServerMap) {
        let addr = "127.0.0.1:6543".parse().unwrap();
        let client_server_map: ClientServerMap = Arc::new(DashMap::new());
        let client = Client {
            read: BufReader::new(empty()),
            write: sink(),
            buffer: PooledBuffer::new(),
            addr,
            addr_str: addr.to_string(),
            read_buf: BytesMut::new(),
            connection_id: 7,
            cancel_mode: false,
            transaction_mode: true,
            sql_prepare_session_pinned: false,
            secret_key: 11,
            client_server_map: client_server_map.clone(),
            stats: Arc::new(ClientStats::default()),
            admin: false,
            last_server_stats: None,
            connected_to_server: true,
            session_xact_start: None,
            pool_name: "db".to_string(),
            username: "user".to_string(),
            cached_pool_id: PoolIdentifier::new("db", "user"),
            migration_pool: None,
            migration_pool_is_dynamic: false,
            server_parameters: ServerParameters::default(),
            prepared: PreparedStatementState::new(true, 0),
            max_memory_usage: u64::MAX,
            client_last_messages_in_tx: PooledBuffer::new(),
            client_pending_begin: None,
            pending_app_name_set: None,
            #[cfg(unix)]
            raw_fd: None,
            #[cfg(all(unix, feature = "tls-migration"))]
            ssl_ptr: None,
        };
        (client, client_server_map)
    }

    #[test]
    fn inner_handler_error_releases_cancel_mapping_before_client_drop() {
        let (mut client, client_server_map) = test_client_with_cancel_map();
        let server_tls = Arc::new(
            ServerTlsConfig::new(ServerTlsMode::Disable, None, None, None)
                .expect("disable TLS config must build"),
        );
        client_server_map.insert(
            (client.connection_id as i32, client.secret_key),
            CancelTarget {
                process_id: 123,
                secret_key: 456,
                host: "127.0.0.1".to_string(),
                port: 5432,
                server_tls,
                connected_with_tls: false,
                pool_name: client.pool_name.clone(),
            },
        );

        client.release_after_inner_handler_error();

        assert!(
            client_server_map
                .get(&(client.connection_id as i32, client.secret_key))
                .is_none(),
            "cancel mapping must be removed before the checked-out backend can be returned"
        );
        assert!(
            !client.connected_to_server,
            "Client::Drop must not later treat the already-detached backend as connected"
        );
    }

    #[test]
    fn cancel_mode_client_drop_preserves_victim_cancel_mapping() {
        // a cancel-mode Client overloads
        // its (connection_id, secret_key) with the VICTIM's pid+secret. The
        // shared Drop must NOT remove that routing row - otherwise the victim
        // becomes un-cancellable for the rest of its in-flight transaction and
        // every retry cancel is a silent no-op.
        let (mut client, client_server_map) = test_client_with_cancel_map();
        client.cancel_mode = true;
        // Cancel-mode clients are never connected to a backend of their own.
        client.connected_to_server = false;
        let server_tls = Arc::new(
            ServerTlsConfig::new(ServerTlsMode::Disable, None, None, None)
                .expect("disable TLS config must build"),
        );
        // The victim's transaction task installed this routing row under the
        // exact key the cancel-mode client now carries.
        client_server_map.insert(
            (client.connection_id as i32, client.secret_key),
            CancelTarget {
                process_id: 123,
                secret_key: 456,
                host: "127.0.0.1".to_string(),
                port: 5432,
                server_tls,
                connected_with_tls: false,
                pool_name: client.pool_name.clone(),
            },
        );
        let key = (client.connection_id as i32, client.secret_key);

        drop(client);

        assert!(
            client_server_map.get(&key).is_some(),
            "dropping a cancel-mode client must NOT remove the victim's cancel-routing row"
        );
    }

    #[test]
    fn named_entries_are_never_evicted_under_anon_pressure() {
        // Anonymous LRU size 1 — but Named must persist regardless.
        let mut cache = PreparedStatementCache::new(1);
        let named_key = PreparedStatementKey::Named("stmt_one".into());
        let _ = cache.put(named_key.clone(), make_cached("stmt_one", "SELECT 1"));

        for i in 0..5 {
            let h = i as u64;
            let _ = cache.put(
                PreparedStatementKey::Anonymous(h),
                make_cached("anon", &format!("SELECT {i}")),
            );
        }

        assert!(cache.get(&named_key).is_some(), "Named entry was evicted");
    }

    #[test]
    fn anonymous_lru_evicts_oldest_when_full() {
        let mut cache = PreparedStatementCache::new(2);
        assert!(matches!(
            cache.put(PreparedStatementKey::Anonymous(1), make_cached("a", "Q1")),
            PutOutcome::Inserted
        ));
        assert!(matches!(
            cache.put(PreparedStatementKey::Anonymous(2), make_cached("a", "Q2")),
            PutOutcome::Inserted
        ));
        let outcome = cache.put(PreparedStatementKey::Anonymous(3), make_cached("a", "Q3"));
        assert!(
            matches!(outcome, PutOutcome::Evicted(_)),
            "Capacity overflow on a fresh hash must yield PutOutcome::Evicted, got {outcome:?}",
        );
        assert!(cache.get(&PreparedStatementKey::Anonymous(1)).is_none());
        assert!(cache.get(&PreparedStatementKey::Anonymous(2)).is_some());
        assert!(cache.get(&PreparedStatementKey::Anonymous(3)).is_some());
    }

    #[test]
    fn anonymous_put_returns_replaced_for_same_hash() {
        // Re-Parsing the same anonymous hash must not signal an eviction —
        // the LRU stays at one entry, no capacity pressure, the operator
        // counter must remain at zero.
        let mut cache = PreparedStatementCache::new(4);
        assert!(matches!(
            cache.put(PreparedStatementKey::Anonymous(42), make_cached("a", "Q")),
            PutOutcome::Inserted
        ));
        let outcome = cache.put(PreparedStatementKey::Anonymous(42), make_cached("a", "Q"));
        assert!(
            matches!(outcome, PutOutcome::Replaced(_)),
            "Same-hash put must yield PutOutcome::Replaced, got {outcome:?}",
        );
        assert_eq!(cache.anonymous_count(), 1);
    }

    #[test]
    fn anonymous_lru_distinguishes_inserted_replaced_evicted() {
        // Capacity 2: walk the three outcomes in sequence.
        let mut cache = PreparedStatementCache::new(2);

        // Two distinct keys → both Inserted.
        assert!(matches!(
            cache.put(PreparedStatementKey::Anonymous(1), make_cached("a", "Q1")),
            PutOutcome::Inserted
        ));
        assert!(matches!(
            cache.put(PreparedStatementKey::Anonymous(2), make_cached("a", "Q2")),
            PutOutcome::Inserted
        ));

        // Re-Parse hash 1 at full capacity → Replaced (no eviction).
        let outcome = cache.put(PreparedStatementKey::Anonymous(1), make_cached("a", "Q1"));
        assert!(
            matches!(outcome, PutOutcome::Replaced(_)),
            "Replacement at capacity must not signal eviction, got {outcome:?}",
        );
        assert_eq!(cache.anonymous_count(), 2);

        // Third distinct hash at full capacity → Evicted; oldest (hash 2)
        // popped because hash 1 was just touched and bumped to MRU.
        let outcome = cache.put(PreparedStatementKey::Anonymous(3), make_cached("a", "Q3"));
        assert!(
            matches!(outcome, PutOutcome::Evicted(_)),
            "Distinct hash at capacity must signal eviction, got {outcome:?}",
        );
        assert!(cache.get(&PreparedStatementKey::Anonymous(2)).is_none());
        assert!(cache.get(&PreparedStatementKey::Anonymous(1)).is_some());
        assert!(cache.get(&PreparedStatementKey::Anonymous(3)).is_some());
    }

    #[test]
    fn named_put_returns_inserted_then_replaced() {
        // Named map is unbounded — capacity-driven eviction never occurs.
        // First put on a fresh name → Inserted; same name again → Replaced.
        let mut cache = PreparedStatementCache::new(0);
        let key = PreparedStatementKey::Named("stmt".into());
        let first = cache.put(key.clone(), make_cached("stmt", "Q1"));
        assert!(
            matches!(first, PutOutcome::Inserted),
            "First Named put on a fresh name must be Inserted, got {first:?}",
        );
        let second = cache.put(key.clone(), make_cached("stmt", "Q2"));
        assert!(
            matches!(second, PutOutcome::Replaced(_)),
            "Re-put on existing Named name must be Replaced, got {second:?}",
        );
        assert_eq!(cache.named_count(), 1);
    }

    #[test]
    fn anonymous_unlimited_when_size_zero() {
        let mut cache = PreparedStatementCache::new(0);
        for i in 0..1000_u64 {
            let outcome = cache.put(PreparedStatementKey::Anonymous(i), make_cached("a", "Q"));
            assert!(
                matches!(outcome, PutOutcome::Inserted),
                "Unlimited cache must not evict or replace on fresh keys, got {outcome:?}",
            );
        }
        assert_eq!(cache.anonymous_count(), 1000);
    }

    #[test]
    fn pop_routes_by_key_kind() {
        let mut cache = PreparedStatementCache::new(0);
        let _ = cache.put(
            PreparedStatementKey::Named("a".into()),
            make_cached("a", "Q"),
        );
        let _ = cache.put(PreparedStatementKey::Anonymous(1), make_cached("b", "Q"));
        assert!(cache
            .pop(&PreparedStatementKey::Named("a".into()))
            .is_some());
        assert!(cache
            .pop(&PreparedStatementKey::Named("a".into()))
            .is_none());
        assert!(cache.pop(&PreparedStatementKey::Anonymous(1)).is_some());
    }

    #[test]
    fn pop_by_server_name_removes_named_and_anonymous_entries() {
        let mut cache = PreparedStatementCache::new(0);
        let named_key = PreparedStatementKey::Named("client_stmt".into());
        let anon_key = PreparedStatementKey::Anonymous(42);
        let _ = cache.put(named_key.clone(), make_cached("DOORMAN_named", "SELECT 1"));
        let _ = cache.put(anon_key.clone(), make_cached("DOORMAN_anon", "SELECT 2"));

        let (removed_named_key, _) = cache
            .pop_by_server_name("DOORMAN_named")
            .expect("named server-side statement should be removed");
        let (removed_anon_key, _) = cache
            .pop_by_server_name("DOORMAN_anon")
            .expect("anonymous server-side statement should be removed");

        assert_eq!(removed_named_key, named_key);
        assert_eq!(removed_anon_key, anon_key);
        assert!(cache.get(&removed_named_key).is_none());
        assert!(cache.get(&removed_anon_key).is_none());
        assert_eq!(cache.memory_usage_approx(), 0);
    }

    #[test]
    fn named_order_is_bounded_under_close_churn_below_cap() {
        let mut cache = PreparedStatementCache::new(0);
        for i in 0..(MAX_NAMED_PREPARED_PER_CLIENT * 3) {
            let name = format!("close_churn_{i}");
            let key = PreparedStatementKey::Named(name.clone());
            let _ = cache.put(key.clone(), make_cached(&name, "SELECT 1"));
            assert!(cache.pop(&key).is_some());
        }

        assert_eq!(cache.named_count(), 0);
        assert_eq!(cache.memory_usage_approx(), 0);
        assert!(
            cache.named_order.len() <= MAX_NAMED_ORDER_QUEUE_PER_CLIENT,
            "named_order retained {} stale keys below the named cache cap",
            cache.named_order.len()
        );
    }

    #[test]
    fn clear_empties_both_maps() {
        let mut cache = PreparedStatementCache::new(0);
        let _ = cache.put(
            PreparedStatementKey::Named("a".into()),
            make_cached("a", "Q"),
        );
        let _ = cache.put(PreparedStatementKey::Anonymous(1), make_cached("b", "Q"));
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.named_count(), 0);
        assert_eq!(cache.anonymous_count(), 0);
    }

    /// the implementation invariant: the O(1) incremental counter
    /// (`memory_usage_approx`) must converge to the same value the O(N)
    /// walk (`memory_usage_walk`) reports for the same cache snapshot,
    /// across every mutation path: Inserted, Replaced (Named and
    /// Anonymous), Evicted (Anonymous LRU), pop, and clear.
    ///
    /// If this assert ever fires it means a future change to `put`/
    /// `pop`/`clear` forgot to keep the counter in lock-step with the
    /// underlying maps - the gauge surfaced in `SHOW POOLS_MEMORY` and
    /// the Prometheus `pg_doorman_clients_prepared_cache_bytes` would
    /// silently drift from reality. NamedEvicted is exercised below by
    /// `named_eviction_converges_after_cap_pressure` because the cap is
    /// large enough that constructing it inline here would slow the
    /// test suite measurably.
    #[test]
    fn walk_approx_converge_through_mutations() {
        let mut cache = PreparedStatementCache::new(4);
        let check = |c: &PreparedStatementCache| {
            let walk = c.memory_usage_walk();
            let approx = c.memory_usage_approx();
            assert_eq!(walk, approx, "walk vs approx diverged");
        };

        check(&cache);

        // Inserted (Named + Anonymous, mixed).
        let _ = cache.put(
            PreparedStatementKey::Named("alpha".into()),
            make_cached("alpha", "SELECT 1"),
        );
        check(&cache);
        let _ = cache.put(
            PreparedStatementKey::Named("beta".into()),
            make_cached("beta", "SELECT 2"),
        );
        check(&cache);
        let _ = cache.put(PreparedStatementKey::Anonymous(1), make_cached("a", "Q1"));
        check(&cache);

        // Replaced (Named - value-only delta).
        let _ = cache.put(
            PreparedStatementKey::Named("alpha".into()),
            make_cached("alpha", "SELECT 1 -- changed comment"),
        );
        check(&cache);

        // Replaced (Anonymous, Limited LRU branch).
        let _ = cache.put(
            PreparedStatementKey::Anonymous(1),
            make_cached("a", "Q1 replaced"),
        );
        check(&cache);

        // Eviction in the Limited Anonymous LRU.
        for h in 2..=10u64 {
            let _ = cache.put(PreparedStatementKey::Anonymous(h), make_cached("a", "Q"));
        }
        check(&cache);

        // pop.
        let _ = cache.pop(&PreparedStatementKey::Named("alpha".into()));
        check(&cache);

        // clear.
        cache.clear();
        check(&cache);
        assert_eq!(cache.memory_usage_approx(), 0);
    }

    /// Named cap-driven eviction must keep the incremental counter in sync.
    /// Constructing
    /// MAX_NAMED_PREPARED_PER_CLIENT + 1 entries inline is the only way
    /// to exercise this branch; kept separate so the more general
    /// `walk_approx_converge_through_mutations` stays fast.
    #[test]
    fn named_eviction_converges_after_cap_pressure() {
        let mut cache = PreparedStatementCache::new(0);
        // Fill to cap.
        for i in 0..MAX_NAMED_PREPARED_PER_CLIENT {
            let name = format!("s_{i}");
            let _ = cache.put(
                PreparedStatementKey::Named(name.clone()),
                make_cached(&name, "SELECT 1"),
            );
        }
        // One more - triggers NamedEvicted branch.
        let extra = format!("s_{MAX_NAMED_PREPARED_PER_CLIENT}");
        let outcome = cache.put(
            PreparedStatementKey::Named(extra.clone()),
            make_cached(&extra, "SELECT 1"),
        );
        assert!(matches!(outcome, PutOutcome::NamedEvicted { .. }));
        assert_eq!(cache.named_count(), MAX_NAMED_PREPARED_PER_CLIENT);
        assert_eq!(
            cache.memory_usage_walk(),
            cache.memory_usage_approx(),
            "NamedEvicted branch broke walk-approx parity"
        );
    }

    #[test]
    fn iter_yields_both_maps() {
        let mut cache = PreparedStatementCache::new(0);
        let _ = cache.put(
            PreparedStatementKey::Named("a".into()),
            make_cached("a", "Q"),
        );
        let _ = cache.put(PreparedStatementKey::Anonymous(1), make_cached("b", "Q"));
        let kinds: Vec<&str> = cache
            .iter()
            .map(|(k, _)| match k {
                PreparedStatementKeyRef::Named(_) => "named",
                PreparedStatementKeyRef::Anonymous(_) => "anon",
            })
            .collect();
        assert_eq!(kinds.len(), 2);
        assert!(kinds.contains(&"named"));
        assert!(kinds.contains(&"anon"));
    }

    #[test]
    fn iter_borrows_named_keys_without_allocation() {
        // Regression guard for B3: iter() must yield borrowed Named keys,
        // not freshly cloned Strings. The yielded &str must point into the
        // map's owned String, so its address must match across calls and
        // not match the address of an unrelated owned copy.
        let mut cache = PreparedStatementCache::new(0);
        let name = "stmt_borrow_check".to_owned();
        let _ = cache.put(
            PreparedStatementKey::Named(name.clone()),
            make_cached("stmt", "SELECT 1"),
        );

        // Two consecutive iter() calls must hand back the same backing pointer.
        let first_ptr = cache
            .iter()
            .find_map(|(k, _)| match k {
                PreparedStatementKeyRef::Named(s) => Some(s.as_ptr()),
                _ => None,
            })
            .expect("Named entry not yielded");
        let second_ptr = cache
            .iter()
            .find_map(|(k, _)| match k {
                PreparedStatementKeyRef::Named(s) => Some(s.as_ptr()),
                _ => None,
            })
            .expect("Named entry not yielded");
        assert_eq!(
            first_ptr, second_ptr,
            "iter() must borrow the same String storage on each call"
        );
        // And it must differ from a freshly-built String — proving we are
        // not silently copying somewhere.
        assert_ne!(first_ptr, name.as_ptr());
    }

    #[test]
    fn iter_handles_fifty_named_entries() {
        // Smoke test: 50-Named-entry cache mirrors a typical ORM client.
        // Counts every yielded entry to catch any regression that would
        // truncate or panic the iterator.
        let mut cache = PreparedStatementCache::new(0);
        for i in 0..50_u32 {
            let _ = cache.put(
                PreparedStatementKey::Named(format!("stmt_{i}")),
                make_cached("stmt", "SELECT 1"),
            );
        }
        assert_eq!(cache.iter().count(), 50);

        let mut named = 0_usize;
        let mut anon = 0_usize;
        for (k, _) in cache.iter() {
            match k {
                PreparedStatementKeyRef::Named(_) => named += 1,
                PreparedStatementKeyRef::Anonymous(_) => anon += 1,
            }
        }
        assert_eq!(named, 50);
        assert_eq!(anon, 0);
    }

    /// Guard for `PreparedStatementsState::discard_clear` - the shared
    /// helper called from both the synthetic DISCARD ALL fast path and
    /// the explicit DEALLOCATE ALL handler. A regression that forgot to
    /// reset one of these fields would let stale prepared-statement
    /// bookkeeping leak across the boundary the client thinks is clean,
    /// resurfacing as SQLSTATE 26000 on the next anonymous Parse.
    #[test]
    fn discard_clear_resets_every_prepared_field() {
        let mut state = PreparedStatementState::new(true, 16);

        // Seed every field we expect `discard_clear` to wipe.
        let named = PreparedStatementKey::Named("warm".into());
        let _ = state
            .cache
            .put(named.clone(), make_cached("warm", "SELECT 1"));
        let _ = state.cache.put(
            PreparedStatementKey::Anonymous(0xdead_beef),
            make_cached("a", "SELECT 2"),
        );
        state.last_anonymous_hash = Some(0xdead_beef);
        state.last_bound_for_top = Some((0x0000_face, true));
        state.skipped_parses.push(SkippedParse {
            statement_name: "DOORMAN_1".into(),
            target: ParseCompleteTarget::BindComplete,
            insert_at_beginning: false,
            has_bind: false,
        });
        state.batch_operations.push(BatchOperation::Execute);
        state
            .portal_set_cleanup_commands
            .insert("portal_set".into(), SetCleanupCommand::GenericSet);
        state
            .portal_reset_cleanup_commands
            .insert("portal_reset".into(), ResetCleanupCommand::ResetAll);
        state
            .disabled_statement_set_cleanup_commands
            .insert("stmt_set".into(), SetCleanupCommand::GenericSet);
        state
            .disabled_statement_reset_cleanup_commands
            .insert("stmt_reset".into(), ResetCleanupCommand::ResetAll);
        state.parses_sent_in_batch = 7;
        // Seed all four response counters too - `reset_batch` zeroes
        // them, `discard_clear` must do the same for symmetry.
        state.processed_response_counts.bind_complete = 5;
        state.processed_response_counts.param_desc = 3;
        state.processed_response_counts.portal_desc = 13;
        state.processed_response_counts.statement_desc_pending = 1;
        state.processed_response_counts.execute = 7;
        state.processed_response_counts.close_complete = 2;
        state.processed_response_counts.parse_complete = 11;
        // Regression seed: pending_close_complete used to escape
        // discard_clear because the simple-query DISCARD ALL path doesn't
        // normally interact with extended-protocol Close. Seed a non-zero
        // value to lock that the reset now happens.
        state.pending_close_complete = 4;

        let cleared = state.discard_clear();

        // The caller logs the count, so it must be the number of entries
        // that actually went away - not zero, not the anonymous-only delta.
        assert_eq!(cleared, 2, "both named and anonymous must be counted");
        assert_eq!(state.cache.len(), 0, "cache must be empty");
        assert_eq!(
            state.named_count(),
            0,
            "no named entries may survive DISCARD ALL"
        );
        assert_eq!(
            state.anonymous_count(),
            0,
            "no anonymous entries may survive DISCARD ALL"
        );
        assert!(
            state.last_anonymous_hash.is_none(),
            "last_anonymous_hash must be cleared so the next anonymous Parse \
             does not dedupe against a forgotten cache entry (SQLSTATE 26000 \
             prevention)"
        );
        assert!(
            state.last_bound_for_top.is_none(),
            "last_bound_for_top must be cleared so /api/top/queries does not \
             attribute the next batch to a forgotten hash"
        );
        assert!(state.skipped_parses.is_empty());
        assert!(state.batch_operations.is_empty());
        assert!(
            state.portal_set_cleanup_commands.is_empty(),
            "set cleanup attribution must not survive a full prepared-state clear"
        );
        assert!(
            state.portal_reset_cleanup_commands.is_empty(),
            "reset cleanup attribution must not survive a full prepared-state clear"
        );
        assert!(
            state.disabled_statement_set_cleanup_commands.is_empty(),
            "disabled-prepared set attribution must not survive a full prepared-state clear"
        );
        assert!(
            state.disabled_statement_reset_cleanup_commands.is_empty(),
            "disabled-prepared reset attribution must not survive a full prepared-state clear"
        );
        assert_eq!(state.parses_sent_in_batch, 0);
        assert_eq!(state.processed_response_counts.parse_complete, 0);
        assert_eq!(
            state.processed_response_counts.bind_complete, 0,
            "bind_complete counter must reset for symmetry with reset_batch"
        );
        assert_eq!(state.processed_response_counts.param_desc, 0);
        assert_eq!(state.processed_response_counts.portal_desc, 0);
        assert_eq!(state.processed_response_counts.statement_desc_pending, 0);
        assert_eq!(state.processed_response_counts.execute, 0);
        assert_eq!(state.processed_response_counts.close_complete, 0);
        assert_eq!(
            state.pending_close_complete, 0,
            "pending_close_complete must reset so a stale CloseComplete \
             cannot be injected into the next batch by the reordering \
             logic in protocol.rs"
        );
    }
}
