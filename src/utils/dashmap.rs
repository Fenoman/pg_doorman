use dashmap::DashMap;
use std::hash::Hash;

/// DashMap variant backed by
/// `ahash::RandomState` instead of the std `RandomState` (SipHash-1-3
/// is a security default - overkill for an in-process cache where the
/// keys are not adversary-controlled hash inputs).
///
/// Bench evidence from VM-wave: `hash/large/xxhash3_structured`
/// ~36 ns / 7+ GiB/s vs `hash/large/default_hasher_structured`
/// 93 ns / 2.86 GiB/s - i.e. SipHash is ~2.5× slower per hash op.
/// `Hasher::write` was visible at 0.71% CPU in the pgbench
/// `-S -M prepared` flamegraph; combined with `DashMap::_get` at
/// 0.38% and `LabelKeyTracker::record` at 0.49% the SipHash
/// machinery dominated the metric-path overhead surfaced after the implementation/H.
///
/// `ahash` is already a project dependency (used by `AHashMap` in
/// `client/core.rs`) and is the standard high-performance non-secure
/// hasher for hot-path hash maps in Rust.
pub type FastDashMap<K, V> = DashMap<K, V, ahash::RandomState>;
const MAX_DASHMAP_SHARDS: usize = 4096;

/// Creates a new DashMap with shard count based on worker_threads.
/// This avoids overhead from incorrect CPU detection in k8s pods.
pub fn new_dashmap<K, V>(worker_threads: usize) -> DashMap<K, V>
where
    K: Eq + Hash,
{
    DashMap::with_shard_amount(optimal_shard_count(worker_threads))
}

/// Creates a new DashMap with capacity and shard count based on worker_threads.
pub fn new_dashmap_with_capacity<K, V>(capacity: usize, worker_threads: usize) -> DashMap<K, V>
where
    K: Eq + Hash,
{
    DashMap::with_capacity_and_shard_amount(capacity, optimal_shard_count(worker_threads))
}

/// ahash-backed variant of `new_dashmap` for hot-path
/// caches (PreparedStatementCache, NAMED/ANON_INTERNER,
/// LabelKeyTracker, PROTOCOL_STATES). Identical shard sizing - only
/// the hasher differs.
pub fn new_fast_dashmap<K, V>(worker_threads: usize) -> FastDashMap<K, V>
where
    K: Eq + Hash,
{
    DashMap::with_hasher_and_shard_amount(
        ahash::RandomState::new(),
        optimal_shard_count(worker_threads),
    )
}

/// ahash-backed variant of `new_dashmap_with_capacity`.
pub fn new_fast_dashmap_with_capacity<K, V>(
    capacity: usize,
    worker_threads: usize,
) -> FastDashMap<K, V>
where
    K: Eq + Hash,
{
    DashMap::with_capacity_and_hasher_and_shard_amount(
        capacity,
        ahash::RandomState::new(),
        optimal_shard_count(worker_threads),
    )
}

/// Calculates optimal shard count based on worker_threads.
/// Uses power of 2 for better hash distribution.
fn optimal_shard_count(worker_threads: usize) -> usize {
    // Minimum 4 shards, maximum based on worker_threads * 4
    // Round up to nearest power of 2 for better hash distribution
    let target = worker_threads
        .saturating_mul(4)
        .clamp(4, MAX_DASHMAP_SHARDS);
    target.next_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimal_shard_count() {
        // worker_threads=1 -> target=4 -> 4 shards
        assert_eq!(optimal_shard_count(1), 4);
        // worker_threads=2 -> target=8 -> 8 shards
        assert_eq!(optimal_shard_count(2), 8);
        // worker_threads=4 -> target=16 -> 16 shards
        assert_eq!(optimal_shard_count(4), 16);
        // worker_threads=8 -> target=32 -> 32 shards
        assert_eq!(optimal_shard_count(8), 32);
        // worker_threads=3 -> target=12 -> 16 shards (next power of 2)
        assert_eq!(optimal_shard_count(3), 16);
    }

    #[test]
    fn optimal_shard_count_handles_extreme_worker_threads() {
        let shards = optimal_shard_count(usize::MAX);
        assert!(shards.is_power_of_two());
        assert_eq!(shards, MAX_DASHMAP_SHARDS);
    }

    #[test]
    fn test_new_dashmap() {
        let map: DashMap<u64, String> = new_dashmap(4);
        assert!(map.is_empty());
        map.insert(1, "test".to_string());
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_new_dashmap_with_capacity() {
        let map: DashMap<u64, String> = new_dashmap_with_capacity(100, 4);
        assert!(map.is_empty());
        map.insert(1, "test".to_string());
        assert_eq!(map.len(), 1);
    }
}
