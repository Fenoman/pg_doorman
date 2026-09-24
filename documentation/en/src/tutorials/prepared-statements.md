# Prepared statements in transaction pooling

PgDoorman keeps a client's prepared statements usable when transactions move
between PostgreSQL backends. Both named and anonymous frontend statements are
mapped to internal `DOORMAN_<N>` names.

## One backend statement per query shape

Parses with the same SQL, parameter OIDs and startup planner settings share one
pool entry and one internal name. When the checked-out backend already holds
that name, PgDoorman answers the `Parse` with a synthetic `ParseComplete` and
does not forward it; otherwise it forwards the `Parse` under that name. A later
`Bind` or `Describe` on a backend that lacks the name first prepares it there,
preserving the frontend response order. A backend therefore holds one statement
per query shape it has served, not one per call or per client. A pool entry
that is evicted and inserted again gets a new internal name; backends keep the
old one until their own LRU or cleanup removes it.

```text
Client A: Parse("old", SQL)   -> PostgreSQL: Parse("DOORMAN_42", SQL)
Client B: Parse("new", SQL)   -> backend holds DOORMAN_42: synthetic ParseComplete
Client A: Bind("old")         -> PostgreSQL: Bind("DOORMAN_42")
Client B: Bind("new")         -> PostgreSQL: Bind("DOORMAN_42")
```

Skipping a repeated `Parse` spares PostgreSQL the parse analysis of a query it
has already prepared. PostgreSQL's choice between custom and generic plans for a
statement on a backend is shared by all clients that use it there, so it can
reflect their mixed parameter values. The pool shares
immutable `Arc<Parse>` metadata, SQL text and parameter OID arrays across
clients.

A client that has sent `Flush` gets a new internal name on every later `Parse`.
Its copies are bounded only by `server_prepared_statements_cache_size` on each
backend. Binary upgrade migration restores the same naming.

## Reserved statement names

Internal names start with `DOORMAN_`. When a command must fail with PostgreSQL's
own error in its place, such as a `Bind` of a statement the client never
prepared, PgDoorman aims it at a `DOORMAN_missing_<N>` that does not exist.
Applications should not create statements with this prefix:

- A simple-query `DEALLOCATE "DOORMAN_..."` does not reach PostgreSQL under
  that name, so a statement created with SQL `PREPARE "DOORMAN_..."` can be
  removed only by `DEALLOCATE ALL`. With `prepared_statements = true` a
  protocol `Close` does not reach it either.
- A statement named `DOORMAN_missing_<N>` can answer a `Bind` of an unknown
  statement instead of the `26000` error.
- A `DEALLOCATE "DOORMAN_<N>"` inside a multi-statement simple query or sent
  through the extended protocol reaches PostgreSQL and drops a statement other
  clients share. The next client to use it on that backend gets a `26000`
  error, and the backend's statements are reset when it returns to the pool.

## Schema changes

A skipped `Parse` is not analyzed again. After DDL that changes the result
shape, such as `ALTER TABLE ... ADD COLUMN` under `SELECT *` or a changed
function result type, the first `Bind` of a statement prepared before the DDL
fails on each backend with `0A000` (`cached plan must not change result type`).
A direct connection would analyze a fresh `Parse` against the new schema.

This error, like a missing (`26000`) or duplicate (`42P05`) statement name,
schedules `DEALLOCATE ALL` for the moment the backend returns to the pool; with
`cleanup_server_connections = false` the backend is closed instead. Other SQL
errors, such as a unique violation or a serialization failure, leave the
backend's statements in place. The exception is such an error in a batch the
client ended with `Flush` instead of `Sync`: it also schedules a reset of
session settings, role and cursors, so with `cleanup_server_connections = false`
this backend is closed too, statements included. After `DEALLOCATE ALL` the
next `Parse` on that backend is prepared against the new schema, so each
backend reports the error once per such DDL.
PgDoorman does not retry SQL execution to hide the error.

The cleanup waits for the backend to return to the pool. Until then a `Parse`
of that query is still answered from the stale statement, and its next `Bind` or
`Describe` fails again: in the same transaction after a rollback to a savepoint
(without one, the aborted transaction reports `25P02`), and in every later
transaction of a session that ran SQL-level `PREPARE`, because such a session
keeps its backend. `RECONNECT <database>` on the admin console closes idle backends at
once and busy ones when they return to the pool.

An old statement bound on a backend that no longer holds it is prepared there
again, so its result shape follows that backend's current schema. Transaction
pooling cannot preserve a descriptor held only in a backend that was cleaned or
retired.

Giving every `Parse` a fresh internal name avoided the `0A000` but left the
previous copy on whichever backend had run it. In transaction pooling that
filled every backend with duplicate plans up to
`server_prepared_statements_cache_size`.

## Cache layers and limits

| Layer | Contents | Bound |
| --- | --- | --- |
| Pool | Shared Parse metadata keyed by SQL, parameter OIDs and startup planner settings | `prepared_statements_cache_size` |
| Client, named | Client name to logical statement and backend alias | 2048 entries per client |
| Client, anonymous | Query-hash entries and the currently addressable unnamed statement | `client_anonymous_prepared_cache_size`; zero selects an unlimited map |
| Backend | Internal names prepared on this PostgreSQL connection | `server_prepared_statements_cache_size` |

An unset client or backend cache size inherits the resolved pool prepared-cache
size. A backend-cache eviction sends Close and later Bind can reprepare the
statement. A pool-metadata eviction does not remove client-held statements.

Replacing a client entry whose backend name differs schedules closure of the
previous name on the current backend after the new Parse succeeds. That happens
when a named statement is re-Parsed with another query, on every re-Parse by a
client that sent `Flush`, and after the pool entry was evicted and inserted
again. Otherwise a re-Parse of the same query keeps the name and closes nothing.
A failed named Parse restores the previous client entry; a failed unnamed Parse
leaves no unnamed statement, as in PostgreSQL.
Anonymous LRU eviction drops the local entry; backend LRU and backend retirement
bound the lifetime of physical statements left on other connections. Named cap
eviction is counted separately from replacement.

The query interner deduplicates SQL text and has its own GC/TTL settings. Its
memory is distinct from PostgreSQL's prepared-plan memory. See the configuration
reference for `query_interner_gc_interval_seconds` and
`query_interner_anon_idle_ttl_seconds`.

## Configuration

```toml
[general]
prepared_statements = true
prepared_statements_cache_size = 8192
server_prepared_statements_cache_size = 1024
client_anonymous_prepared_cache_size = 256
```

These are example capacities, not workload-independent recommendations. Size
the backend cache from measured plan memory and reprepare rates. Size the client
cache from the session's working set. `max_memory_usage` bounds in-flight
processing buffers; it is not a prepared-cache memory limit.

With `prepared_statements = false`, protocol messages use PostgreSQL's original
statement names. Turning it off changes the transaction-pooling compatibility
of named statements; it is not a substitute for measuring the configured mode.

## Observability

`SHOW POOLS_MEMORY` reports pool/client cache state. `SHOW PREPARED_STATEMENTS`
and the prepared-statements web views describe shared pool entries; their names
are the physical backend names except for clients that sent `Flush`.

The pool prepared-entry hit/miss counters record the path chosen for each
`Parse`: a hit means PgDoorman answered it without forwarding, because the
backend holds the statement or an earlier `Parse` of the same batch prepares
it; a miss means the `Parse` was forwarded. Backend cache hits also count later
Bind/Describe operations.

Use `pg_prepared_statements` on the backend being inspected to see its actual
physical names and plan counts. The client Anonymous and Named eviction metrics
measure their respective capacity pressure; repeated replacement of the same
anonymous entry does not increment the Named eviction counter.

After rolling out a new version, check `SHOW SERVERS` under production-like
load. `prepare_cache_size` of each backend should settle near the number of
distinct queries the application sends. A value that climbs towards
`server_prepared_statements_cache_size` means backends accumulate duplicate
plans at the cost of PostgreSQL memory. For drivers that Parse on every call the
same defect lowers the server prepared hit ratio, which the
**`PgDoormanPreparedHitRatioLow`** alert in
`monitoring/prometheus-rules/prepared-statements.yaml` watches.

## Reference

- [Pool Modes](../concepts/pool-modes.md)
- [General Settings](../reference/general.md)
- [Admin Commands](../observability/admin-commands.md)
- [Prometheus](../reference/prometheus.md)
- [Query interner monitoring](../operations/monitoring-interner.md)
