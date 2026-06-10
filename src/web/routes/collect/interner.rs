use crate::server::{anon_snapshot, anon_stats, named_snapshot, named_stats, now_monotonic_ms};
use crate::web::routes::dto::{InternerDto, InternerKindDto, InternerTopDto, InternerTopRowDto};

use super::{clamp_top_n, now_unix_ms};

pub(crate) fn collect_interner() -> InternerDto {
    let named = named_stats();
    let anon = anon_stats();

    InternerDto {
        ts: now_unix_ms(),
        named: InternerKindDto {
            entries: named.entries,
            bytes: named.bytes,
        },
        anonymous: InternerKindDto {
            entries: anon.entries,
            bytes: anon.bytes,
        },
    }
}

pub(crate) fn collect_interner_top(n: u64) -> InternerTopDto {
    let n = clamp_top_n(n);
    let now = now_monotonic_ms();

    enum Handle {
        Named(std::sync::Arc<crate::server::NamedEntry>),
        Anon(std::sync::Arc<crate::server::AnonEntry>),
    }

    let mut combined: Vec<(u64, &'static str, usize, i64, Handle)> = Vec::new();
    for (hash, entry) in named_snapshot() {
        let bytes = entry.text().len();
        combined.push((hash, "named", bytes, -1, Handle::Named(entry)));
    }
    for (hash, entry) in anon_snapshot() {
        let idle = entry.idle_ms(now) as i64;
        let bytes = entry.text().len();
        combined.push((hash, "anonymous", bytes, idle, Handle::Anon(entry)));
    }
    combined.sort_by_key(|r| std::cmp::Reverse(r.2));

    let entries = combined
        .into_iter()
        .take(n as usize)
        .map(|(hash, kind, bytes, idle_ms, handle)| {
            let text = match handle {
                Handle::Named(e) => e.text().clone(),
                Handle::Anon(e) => e.text().clone(),
            };
            let preview = crate::utils::strings::preview_query(&text);
            InternerTopRowDto {
                hash: format!("{hash:#x}"),
                kind: kind.to_string(),
                bytes: bytes as u64,
                idle_ms,
                preview,
            }
        })
        .collect();

    InternerTopDto {
        ts: now_unix_ms(),
        n,
        entries,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn aggregate_collector_does_not_materialize_interner_snapshots() {
        let src = include_str!("interner.rs");
        let start = src
            .find("pub(crate) fn collect_interner()")
            .expect("aggregate collector must exist");
        let end = src
            .find("pub(crate) fn collect_interner_top")
            .expect("top collector should follow aggregate collector");
        let body = &src[start..end];

        assert!(
            body.contains("named_stats()"),
            "aggregate endpoint must use O(1) named interner stats"
        );
        assert!(
            body.contains("anon_stats()"),
            "aggregate endpoint must use O(1) anonymous interner stats"
        );
        assert!(
            !body.contains("named_snapshot()") && !body.contains("anon_snapshot()"),
            "public aggregate endpoint must not clone full interner snapshots"
        );
    }
}
