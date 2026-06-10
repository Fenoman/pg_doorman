use crate::pool::get_all_pools;
use crate::web::routes::dto::{PreparedDto, PreparedRowDto, PreparedTextDto};

use super::now_unix_ms;

const PREPARED_MAX_ROWS: usize = 10_000;

pub(crate) fn collect_prepared() -> PreparedDto {
    let mut prepared: Vec<PreparedRowDto> = Vec::new();
    let mut truncated = false;
    'pools: for (identifier, pool) in get_all_pools().iter() {
        let Some(cache) = pool.prepared_statement_cache.as_ref() else {
            continue;
        };
        let completed =
            cache.for_each_entry_until(|hash, parse, count_used, kind, hits, misses| {
                if prepared.len() >= PREPARED_MAX_ROWS {
                    truncated = true;
                    return false;
                }
                prepared.push(PreparedRowDto {
                    pool: identifier.to_string(),
                    hash: hash.to_string(),
                    name: parse.name.clone(),
                    count_used,
                    hits,
                    misses,
                    kind: kind.as_str().to_string(),
                });
                true
            });
        if !completed {
            break 'pools;
        }
    }

    // Stable order: pool first, then hash, for deterministic UI display.
    prepared.sort_by(|a, b| {
        (a.pool.as_str(), a.hash.as_str()).cmp(&(b.pool.as_str(), b.hash.as_str()))
    });

    PreparedDto {
        ts: now_unix_ms(),
        truncated,
        prepared,
    }
}

pub(crate) fn collect_prepared_text(hash: u64) -> Option<PreparedTextDto> {
    for (identifier, pool) in get_all_pools().iter() {
        let Some(cache) = pool.prepared_statement_cache.as_ref() else {
            continue;
        };
        if let Some((parse, kind)) = cache.lookup_by_hash(hash) {
            return Some(PreparedTextDto {
                ts: now_unix_ms(),
                hash: format!("{hash:#x}"),
                pool: identifier.to_string(),
                name: parse.name.clone(),
                query: parse.query().to_string(),
                kind: kind.as_str().to_string(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn collect_prepared_uses_bounded_cache_iteration() {
        let src = include_str!("prepared.rs");
        let start = src
            .find("pub(crate) fn collect_prepared()")
            .expect("collect_prepared function not found");
        let end = src[start..]
            .find("pub(crate) fn collect_prepared_text")
            .expect("collect_prepared_text function not found");
        let body = &src[start..start + end];

        assert!(
            !body.contains("get_entries()"),
            "collect_prepared must not clone every prepared cache entry before applying a cap"
        );
        assert!(
            body.contains("for_each_entry_until"),
            "collect_prepared should iterate with early stop support"
        );
        assert!(
            body.contains("PREPARED_MAX_ROWS"),
            "collect_prepared should have an explicit response row cap"
        );
    }
}
