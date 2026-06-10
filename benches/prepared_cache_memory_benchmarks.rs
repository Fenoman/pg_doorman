//! Benchmarks the O(N) prepared-cache memory walk against the O(1)
//! incremental atomic counter used on the hot Parse path.
//!
//! Hot path: `update_prepared_cache_stats` is called from
//! `client/protocol.rs` and `client/transaction.rs` on every Parse.
//! With the default 8192-entry Anonymous LRU and 10k Parse/sec this
//! walk drove ~80M atomic loads/sec (one per cached entry per Parse).
//!
//! The bench seeds caches of 1, 10, 100, 1000, 8192 entries and times
//! both code paths so the speedup ratio is visible per cache size.

use bytes::{BufMut, BytesMut};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::sync::Arc;

use pg_doorman::client::{CachedStatement, PreparedStatementCache, PreparedStatementKey};
use pg_doorman::messages::Parse;

fn make_parse(name: &str, query: &str) -> Arc<Parse> {
    let mut buf = BytesMut::new();
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
    let parse: Parse = (&buf).try_into().unwrap();
    Arc::new(parse)
}

fn make_entry(name: &str, query: &str) -> CachedStatement {
    CachedStatement::new(make_parse(name, query), 0, None)
}

fn seed_named(count: usize) -> PreparedStatementCache {
    let mut cache = PreparedStatementCache::new(0);
    for i in 0..count {
        let name = format!("stmt_{i:05}");
        let _ = cache.put(
            PreparedStatementKey::Named(name.clone()),
            make_entry(&name, "SELECT 1"),
        );
    }
    cache
}

fn seed_anonymous(count: usize) -> PreparedStatementCache {
    // 0 → unlimited Anonymous map, mirrors the hot-path default.
    let mut cache = PreparedStatementCache::new(0);
    for i in 0..count {
        let _ = cache.put(
            PreparedStatementKey::Anonymous(i as u64),
            make_entry("", "SELECT 1"),
        );
    }
    cache
}

fn bench_memory_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("prepared_cache_memory_usage");
    group.throughput(Throughput::Elements(1));

    for &count in &[1usize, 10, 100, 1000, 8192] {
        let named_cache = seed_named(count);
        let anon_cache = seed_anonymous(count);

        // O(N) walk. Excludes the
        // `Arc::strong_count == 1` term so this is an apples-to-apples
        // bound on the new counter (the additional term was the same
        // ~one extra atomic load per entry).
        group.bench_with_input(BenchmarkId::new("walk/named", count), &count, |b, _| {
            b.iter(|| std::hint::black_box(named_cache.memory_usage_walk()))
        });
        group.bench_with_input(BenchmarkId::new("walk/anonymous", count), &count, |b, _| {
            b.iter(|| std::hint::black_box(anon_cache.memory_usage_walk()))
        });

        // O(1) atomic load on the hot path.
        group.bench_with_input(BenchmarkId::new("approx/named", count), &count, |b, _| {
            b.iter(|| std::hint::black_box(named_cache.memory_usage_approx()))
        });
        group.bench_with_input(
            BenchmarkId::new("approx/anonymous", count),
            &count,
            |b, _| b.iter(|| std::hint::black_box(anon_cache.memory_usage_approx())),
        );
    }

    group.finish();
}

criterion_group!(benches, bench_memory_usage);
criterion_main!(benches);
