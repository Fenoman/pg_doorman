use dashmap::DashMap;
use log::info;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use crate::messages::Parse;
use crate::utils::dashmap::new_dashmap_with_capacity;

/// Global query string interner.
/// This ensures that identical query texts share the same Arc<str> allocation,
/// while avoiding a permanent strong-reference root that would grow RSS forever.
///
/// Entries are stored as Weak<str> and are cleaned lazily, so query text is kept only
/// while some live Parse still references it.
static QUERY_INTERNER: Lazy<DashMap<u64, Weak<str>>> = Lazy::new(|| DashMap::with_capacity(8192));
static QUERY_INTERNER_INSERTIONS: AtomicU64 = AtomicU64::new(0);
const QUERY_INTERNER_CLEANUP_INTERVAL: u64 = 1024;
const QUERY_INTERNER_MIN_CLEANUP_LEN: usize = 8192;

fn cleanup_query_interner() {
    if QUERY_INTERNER.len() < QUERY_INTERNER_MIN_CLEANUP_LEN {
        return;
    }
    QUERY_INTERNER.retain(|_, value| value.strong_count() > 0);
}

/// Interns a query string, returning a shared Arc<str>.
/// If the query was already interned, returns the existing Arc<str>.
/// This is used to ensure query texts are shared between all Parse instances.
///
/// Uses DashMap entry API to avoid a race where two threads intern the same
/// query simultaneously and end up with separate Arc<str> allocations.
pub fn intern_query(query: &str, hash: u64) -> Arc<str> {
    // Fast path (lock-free read): reuse live interned string if it is still around.
    if let Some(existing) = QUERY_INTERNER.get(&hash) {
        if let Some(interned) = existing.value().upgrade() {
            if &*interned == query {
                return interned;
            }
        }
    }

    // Slow path: use entry API for atomic check-and-insert to prevent races.
    let arc_str: Arc<str> = Arc::from(query);
    let weak = Arc::downgrade(&arc_str);

    match QUERY_INTERNER.entry(hash) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            // Another thread may have inserted between our get() and entry() call.
            if let Some(existing) = entry.get().upgrade() {
                if &*existing == query {
                    return existing;
                }
            }
            // Weak expired or hash collision — replace with our new entry.
            entry.insert(weak);
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(weak);
        }
    }

    let insertions = QUERY_INTERNER_INSERTIONS.fetch_add(1, Ordering::Relaxed) + 1;
    if insertions % QUERY_INTERNER_CLEANUP_INTERVAL == 0 {
        cleanup_query_interner();
    }

    arc_str
}

/// Entry in the prepared statement cache with LRU ordering.
struct CacheEntry {
    parse: Arc<Parse>,
    /// Counter for LRU ordering (higher = more recently used)
    count_used: u64,
}

// TODO: Add stats the this cache
// TODO: Add application name to the cache value to help identify which application is using the cache
// TODO: Create admin command to show which statements are in the cache

/// Concurrent prepared statement cache using DashMap with approximate LRU eviction.
///
/// This implementation provides lock-free reads and fine-grained locking for writes,
/// significantly reducing contention compared to a global Mutex<LruCache>.
pub struct PreparedStatementCache {
    cache: DashMap<u64, CacheEntry>,
    /// Maximum number of entries in the cache
    max_size: usize,
    /// Global counter for LRU ordering
    counter: AtomicU64,
}

impl std::fmt::Debug for PreparedStatementCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedStatementCache")
            .field("size", &self.cache.len())
            .field("max_size", &self.max_size)
            .finish()
    }
}

impl PreparedStatementCache {
    pub fn new(mut size: usize, worker_threads: usize) -> Self {
        // Cannot be zero
        if size == 0 {
            size = 1;
        }

        PreparedStatementCache {
            cache: new_dashmap_with_capacity(size, worker_threads),
            max_size: size,
            counter: AtomicU64::new(0),
        }
    }

    /// Adds the prepared statement to the cache if it doesn't exist with a new name
    /// if it already exists will give you the existing parse
    ///
    /// Pass the hash to this so that we can do the compute before acquiring the lock
    pub fn get_or_insert(&self, parse: &Parse, hash: u64) -> Arc<Parse> {
        let timestamp = self.counter.fetch_add(1, Ordering::Relaxed);

        // Fast path: check if already exists.
        // A hash collision must not reuse another client's Parse message.
        if let Some(mut entry) = self.cache.get_mut(&hash) {
            if entry.parse.query() == parse.query()
                && entry.parse.param_types() == parse.param_types()
            {
                entry.count_used = timestamp;
                return entry.parse.clone();
            }
        }

        // Slow path: insert new entry
        // First intern the query string so it's shared across all clients,
        // then rewrite the statement name
        let new_parse = Arc::new(parse.clone().intern_query(hash).rewrite());

        // Insert first, then evict excess. Reversing the order closes
        // the race where N concurrent callers all pass len() >= max_size
        // before any eviction runs, pushing the cache far above the limit.
        self.cache.insert(
            hash,
            CacheEntry {
                parse: new_parse.clone(),
                count_used: timestamp,
            },
        );

        while self.cache.len() > self.max_size {
            self.evict_oldest();
        }

        new_parse
    }

    /// Returns number of entries in the cache
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Returns true if the cache is empty
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Approximate memory usage of the cache in bytes
    pub fn memory_usage(&self) -> usize {
        let mut total = 0;
        for entry in self.cache.iter() {
            total += entry.parse.memory_usage();
            total += std::mem::size_of::<u64>(); // Key
            total += std::mem::size_of::<CacheEntry>();
        }
        total
    }

    /// Returns a list of all entries in the cache
    pub fn get_entries(&self) -> Vec<(u64, Arc<Parse>, u64)> {
        self.cache
            .iter()
            .map(|entry| (*entry.key(), entry.parse.clone(), entry.count_used))
            .collect()
    }

    /// Marks the hash as most recently used if it exists
    pub fn promote(&self, hash: &u64) {
        if let Some(mut entry) = self.cache.get_mut(hash) {
            entry.count_used = self.counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Evict the least recently used entry from the cache.
    fn evict_oldest(&self) {
        let mut oldest_key: Option<u64> = None;
        let mut oldest_time = u64::MAX;

        for entry in self.cache.iter() {
            if entry.count_used < oldest_time {
                oldest_time = entry.count_used;
                oldest_key = Some(*entry.key());
            }
        }

        if let Some(key) = oldest_key {
            if let Some((_, entry)) = self.cache.remove(&key) {
                let query = entry.parse.query();
                let truncated_query = if query.chars().count() > 100 {
                    format!("{}...", query.chars().take(100).collect::<String>())
                } else {
                    query.to_string()
                };
                info!(
                    "evicting prepared statement from cache: name='{}', query='{}'",
                    entry.parse.name, truncated_query
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{BufMut, BytesMut};
    use std::sync::Arc;

    /// Build a minimal Parse message for testing.
    fn make_parse(name: &str, query: &str) -> Parse {
        let mut buf = BytesMut::new();
        buf.put_u8(b'P');
        let name_bytes = name.as_bytes();
        let query_bytes = query.as_bytes();
        // len = 4 (self) + name + null + query + null + 2 (num_params)
        let len = 4 + name_bytes.len() + 1 + query_bytes.len() + 1 + 2;
        buf.put_i32(len as i32);
        buf.put_slice(name_bytes);
        buf.put_u8(0); // null terminator
        buf.put_slice(query_bytes);
        buf.put_u8(0); // null terminator
        buf.put_i16(0); // no params
        Parse::try_from(&buf).unwrap()
    }

    /// Compute hash the same way callers do.
    fn hash_query(query: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        query.hash(&mut h);
        h.finish()
    }

    /// Concurrent inserts may temporarily overshoot max_size by the number
    /// of concurrent inserters, but must not grow without bound.
    #[test]
    fn concurrent_inserts_bounded_overshoot() {
        let max = 50;
        let cache = Arc::new(PreparedStatementCache::new(max, 4));
        let threads = 20;
        let inserts_per_thread = 10; // total 200 unique inserts into cache of 50

        let barrier = Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let cache = cache.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..inserts_per_thread {
                        let query = format!("SELECT {} FROM t{}", i, t);
                        let hash = hash_query(&query);
                        let parse = make_parse("stmt", &query);
                        cache.get_or_insert(&parse, hash);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let final_size = cache.len();
        // Overshoot is bounded by the number of concurrent threads.
        // Without the fix, this was 160 (3.2x max_size).
        let allowed = max + threads;
        assert!(
            final_size <= allowed,
            "cache size {} exceeded allowed {} (max_size {} + {} threads)",
            final_size,
            allowed,
            max,
            threads,
        );
    }

    #[test]
    fn test_query_interner_does_not_keep_strong_references() {
        let hash = 0xD00D_F00D_u64;
        let query = "select 1";

        let interned = intern_query(query, hash);
        assert_eq!(std::sync::Arc::strong_count(&interned), 1);

        let weak = QUERY_INTERNER
            .get(&hash)
            .expect("query should be interned")
            .clone();

        drop(interned);
        assert!(
            weak.upgrade().is_none(),
            "global interner must not keep a strong reference alive"
        );
    }

    #[test]
    fn hash_collision_does_not_reuse_wrong_parse() {
        let cache = PreparedStatementCache::new(10, 1);
        let hash = 0xCAFE_BABE_u64;
        let first = make_parse("stmt1", "SELECT 1");
        let second = make_parse("stmt2", "SELECT 2");

        let cached_first = cache.get_or_insert(&first, hash);
        let cached_second = cache.get_or_insert(&second, hash);

        assert_eq!(cached_first.query(), "SELECT 1");
        assert_eq!(cached_second.query(), "SELECT 2");
        assert_ne!(cached_first.name, cached_second.name);
    }
}
