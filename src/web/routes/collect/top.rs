use std::sync::{atomic::Ordering, Arc};

use crate::pool::get_all_pools;
use crate::server::{anon_snapshot, named_snapshot};
use crate::stats::get_client_stats;
use crate::utils::strings::preview_query;
use crate::web::routes::dto::{
    TopClientBy, TopClientFilters, TopClientRowDto, TopClientsDto, TopPreparedBy, TopPreparedDto,
    TopPreparedFilters, TopPreparedRowDto, TopQueriesDto, TopQueryBy, TopQueryFilters,
    TopQueryRowDto,
};

use super::{clamp_top_clients_n, now_unix_ms};

pub(crate) fn collect_top_prepared(filters: &TopPreparedFilters) -> TopPreparedDto {
    let n = clamp_top_clients_n(filters.n);

    let mut rows: Vec<TopPreparedRowDto> = Vec::new();
    let mut seen = 0usize;
    for (identifier, pool) in get_all_pools().iter() {
        let Some(cache) = pool.prepared_statement_cache.as_ref() else {
            continue;
        };
        cache.for_each_entry_until(|hash, parse, count_used, kind, hits, misses| {
            seen += 1;
            let row = TopPreparedRowDto {
                pool: identifier.to_string(),
                hash: hash.to_string(),
                name: parse.name.clone(),
                count_used,
                hits,
                misses,
                kind: kind.as_str().to_string(),
            };
            push_top_prepared_candidate(&mut rows, row, n as usize, filters.by);
            true
        });
    }

    rows.sort_by(|a, b| match filters.by {
        TopPreparedBy::Hits => b.hits.cmp(&a.hits),
        TopPreparedBy::Misses => b.misses.cmp(&a.misses),
    });

    TopPreparedDto {
        ts: now_unix_ms(),
        by: filters.by.as_str().to_string(),
        n,
        truncated: seen > rows.len(),
        prepared: rows,
    }
}

fn push_top_prepared_candidate(
    rows: &mut Vec<TopPreparedRowDto>,
    row: TopPreparedRowDto,
    max_rows: usize,
    by: TopPreparedBy,
) {
    if max_rows == 0 {
        return;
    }
    if rows.len() < max_rows {
        rows.push(row);
        return;
    }
    let row_score = top_prepared_score(&row, by);
    let Some((worst_idx, worst_score)) = rows
        .iter()
        .enumerate()
        .map(|(idx, existing)| (idx, top_prepared_score(existing, by)))
        .min_by_key(|(_, score)| *score)
    else {
        return;
    };
    if row_score > worst_score {
        rows[worst_idx] = row;
    }
}

fn top_prepared_score(row: &TopPreparedRowDto, by: TopPreparedBy) -> u64 {
    match by {
        TopPreparedBy::Hits => row.hits,
        TopPreparedBy::Misses => row.misses,
    }
}

/// Partition `rows` so the first `n` items are the top-N according to
/// `cmp`, then sort just those for stable display order. Avoids the
/// O(n log n) cost of fully sorting a 10k-entry interner snapshot when
/// the operator only needs the leading 20.
fn truncate_top_n<T, F>(rows: &mut Vec<T>, n: usize, mut cmp: F)
where
    F: FnMut(&T, &T) -> std::cmp::Ordering,
{
    if rows.len() <= n {
        rows.sort_by(&mut cmp);
        return;
    }
    // select_nth_unstable_by partitions in O(n); the truncate that
    // follows is the actual size cap, and the final sort runs against
    // the n winners only.
    rows.select_nth_unstable_by(n, &mut cmp);
    rows.truncate(n);
    rows.sort_by(&mut cmp);
}

pub(crate) fn collect_top_clients(filters: &TopClientFilters) -> TopClientsDto {
    let snapshot: Vec<_> = get_client_stats().values().cloned().collect();
    top_clients_from(snapshot, filters)
}

fn top_clients_from(
    snapshot: Vec<std::sync::Arc<crate::stats::ClientStats>>,
    filters: &TopClientFilters,
) -> TopClientsDto {
    let n = clamp_top_clients_n(filters.n);

    let mut rows: Vec<TopClientRowDto> = snapshot
        .iter()
        .filter(|s| {
            if let Some(p) = &filters.pool {
                let id = format!("{}@{}", s.username(), s.pool_name());
                if id != *p {
                    return false;
                }
            }
            true
        })
        .map(|s| {
            let age_seconds = s.connect_time().elapsed().as_secs();
            let queries_total = s.query_count.load(Ordering::Relaxed);
            let errors_total = s.error_count.load(Ordering::Relaxed);
            let qps = queries_total as f64 / age_seconds.max(1) as f64;
            TopClientRowDto {
                client_id: format!("#c{}", s.connection_id()),
                application_name: s.application_name().to_string(),
                user: s.username().to_string(),
                database: s.pool_name().to_string(),
                addr: s.ipaddr().to_string(),
                age_seconds,
                queries_total,
                errors_total,
                qps,
            }
        })
        .collect();

    truncate_top_n(&mut rows, n as usize, |a, b| {
        // All Top-N sorts are descending — operators want busiest first.
        match filters.by {
            TopClientBy::Qps => b
                .qps
                .partial_cmp(&a.qps)
                .unwrap_or(std::cmp::Ordering::Equal),
            TopClientBy::Errors => b.errors_total.cmp(&a.errors_total),
            TopClientBy::Age => b.age_seconds.cmp(&a.age_seconds),
        }
    });

    TopClientsDto {
        ts: now_unix_ms(),
        by: filters.by.as_str().to_string(),
        n,
        clients: rows,
    }
}

pub(crate) fn collect_top_queries(filters: &TopQueryFilters) -> TopQueriesDto {
    collect_top_queries_with_preview(filters, preview_query)
}

struct TopQueryCandidate {
    hash: u64,
    kind: &'static str,
    text: Arc<str>,
    count: u64,
    total_duration_us: u64,
    avg_duration_ms: f64,
}

impl TopQueryCandidate {
    fn new(
        hash: u64,
        kind: &'static str,
        text: Arc<str>,
        count: u64,
        total_duration_us: u64,
    ) -> Self {
        let avg_duration_ms = if count == 0 {
            0.0
        } else {
            total_duration_us as f64 / count as f64 / 1_000.0
        };
        Self {
            hash,
            kind,
            text,
            count,
            total_duration_us,
            avg_duration_ms,
        }
    }
}

fn collect_top_queries_with_preview<F>(filters: &TopQueryFilters, mut preview: F) -> TopQueriesDto
where
    F: FnMut(&str) -> String,
{
    let n = clamp_top_clients_n(filters.n);

    let mut candidates: Vec<TopQueryCandidate> = Vec::new();

    for (hash, entry) in named_snapshot() {
        let count = entry.count();
        let total_duration_us = entry.total_duration_us();
        candidates.push(TopQueryCandidate::new(
            hash,
            "named",
            Arc::clone(entry.text()),
            count,
            total_duration_us,
        ));
    }
    for (hash, entry) in anon_snapshot() {
        let count = entry.count();
        let total_duration_us = entry.total_duration_us();
        candidates.push(TopQueryCandidate::new(
            hash,
            "anonymous",
            Arc::clone(entry.text()),
            count,
            total_duration_us,
        ));
    }

    truncate_top_n(&mut candidates, n as usize, |a, b| match filters.by {
        TopQueryBy::Count => b.count.cmp(&a.count),
        TopQueryBy::Duration => b
            .avg_duration_ms
            .partial_cmp(&a.avg_duration_ms)
            .unwrap_or(std::cmp::Ordering::Equal),
    });

    let rows = candidates
        .into_iter()
        .map(|candidate| TopQueryRowDto {
            hash: format!("{:#x}", candidate.hash),
            kind: candidate.kind.to_string(),
            query: preview(candidate.text.as_ref()),
            count: candidate.count,
            total_duration_us: candidate.total_duration_us,
            avg_duration_ms: candidate.avg_duration_ms,
        })
        .collect();

    TopQueriesDto {
        ts: now_unix_ms(),
        by: filters.by.as_str().to_string(),
        n,
        queries: rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::client::ClientStats;
    use crate::utils::clock;
    use crate::web::routes::dto::TopClientBy;
    use std::sync::Arc;

    fn make_client(
        connection_id: u64,
        db: &str,
        user: &str,
        app: &str,
        queries: u64,
        errors: u64,
    ) -> Arc<ClientStats> {
        let stats = Arc::new(ClientStats::new(
            connection_id,
            app,
            user,
            db,
            "127.0.0.1",
            clock::now(),
            false,
        ));
        stats.query_count.store(queries, Ordering::Relaxed);
        stats.error_count.store(errors, Ordering::Relaxed);
        stats
    }

    #[test]
    fn top_clients_sort_by_errors_desc() {
        let clients = vec![
            make_client(1, "db", "u", "a", 0, 5),
            make_client(2, "db", "u", "a", 0, 1),
            make_client(3, "db", "u", "a", 0, 3),
        ];
        let f = TopClientFilters {
            by: TopClientBy::Errors,
            n: 10,
            pool: None,
        };
        let result = top_clients_from(clients, &f);
        let errs: Vec<u64> = result.clients.iter().map(|c| c.errors_total).collect();
        assert_eq!(errs, vec![5, 3, 1]);
        assert_eq!(result.by, "errors");
    }

    #[test]
    fn top_clients_n_default_when_zero() {
        let clients: Vec<_> = (0..5)
            .map(|i| make_client(i, "db", "u", "a", 0, 0))
            .collect();
        let f = TopClientFilters {
            by: TopClientBy::Qps,
            n: 0,
            pool: None,
        };
        let result = top_clients_from(clients, &f);
        assert_eq!(result.n, 20);
    }

    #[test]
    fn top_clients_pool_filter_excludes_others() {
        let clients = vec![
            make_client(1, "db1", "alice", "a", 0, 0),
            make_client(2, "db2", "bob", "a", 0, 0),
        ];
        let f = TopClientFilters {
            by: TopClientBy::Qps,
            n: 10,
            pool: Some("alice@db1".to_string()),
        };
        let result = top_clients_from(clients, &f);
        assert_eq!(result.clients.len(), 1);
        assert_eq!(result.clients[0].user, "alice");
    }

    #[test]
    #[serial_test::serial(query_interner)]
    fn top_queries_previews_only_top_n_survivors() {
        crate::server::reset_interners_for_test();

        for i in 0..8_u64 {
            let query = format!("select {i}");
            crate::server::intern_query(&query, i + 1, false);
            for _ in 0..i {
                crate::server::record_query_count(i + 1, false);
            }
        }

        let filters = TopQueryFilters {
            by: TopQueryBy::Count,
            n: 3,
        };
        let mut preview_calls = 0;
        let result = collect_top_queries_with_preview(&filters, |query| {
            preview_calls += 1;
            query.to_string()
        });

        assert_eq!(result.queries.len(), 3);
        assert_eq!(
            preview_calls, 3,
            "preview must be built only after top-N truncation"
        );
        assert_eq!(
            result
                .queries
                .iter()
                .map(|row| row.count)
                .collect::<Vec<_>>(),
            vec![7, 6, 5]
        );
    }

    fn make_top_prepared_row(hits: u64, misses: u64) -> TopPreparedRowDto {
        TopPreparedRowDto {
            pool: "pool".to_string(),
            hash: hits.to_string(),
            name: "stmt".to_string(),
            count_used: 0,
            hits,
            misses,
            kind: "named".to_string(),
        }
    }

    #[test]
    fn push_top_prepared_candidate_keeps_only_requested_winners() {
        let mut rows = Vec::new();
        for hits in 0..8 {
            push_top_prepared_candidate(
                &mut rows,
                make_top_prepared_row(hits, 0),
                3,
                TopPreparedBy::Hits,
            );
        }
        rows.sort_by(|a, b| b.hits.cmp(&a.hits));

        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().map(|row| row.hits).collect::<Vec<_>>(),
            vec![7, 6, 5]
        );
    }

    #[test]
    fn collect_top_prepared_does_not_materialize_every_cache_entry() {
        let src = include_str!("top.rs");
        let start = src
            .find("pub(crate) fn collect_top_prepared")
            .expect("collect_top_prepared function not found");
        let end = src[start..]
            .find("/// Partition `rows`")
            .expect("truncate_top_n marker not found");
        let body = &src[start..start + end];

        assert!(
            !body.contains("get_entries()"),
            "collect_top_prepared must not clone every prepared cache entry before top-N truncation"
        );
        assert!(
            body.contains("for_each_entry_until"),
            "collect_top_prepared should stream entries from each cache"
        );
        assert!(
            body.contains("push_top_prepared_candidate"),
            "collect_top_prepared should keep only bounded top-N candidates"
        );
    }
}
