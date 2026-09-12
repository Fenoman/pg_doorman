# Prepared statements in transaction pooling

PgDoorman keeps a client's prepared statements usable when transactions move
between PostgreSQL backends. Both named and anonymous frontend statements are
mapped to internal `DOORMAN_<N>` names.

## A new Parse creates a new backend statement

Every frontend `Parse` handled by the prepared-statement cache is forwarded to
PostgreSQL under a fresh internal name. PostgreSQL supplies `ParseComplete` and
validates the query against the schema visible to that connection. Equal SQL,
parameter OIDs and startup planner settings allow shared pool metadata; they do
not justify reusing another statement's result descriptor.

This distinction matters after `ALTER TABLE`, temporary-table recreation,
function result changes, or DDL from another connection. A new named or anonymous
`Parse` sees the new shape just as a direct PostgreSQL connection does. Rolling
back DDL and preparing again also uses the restored schema.

An existing statement keeps its original backend name. Binding it after an
incompatible result-shape change can still produce PostgreSQL's `0A000`
(`cached plan must not change result type`). A new Parse does not overwrite that
old statement, and PgDoorman does not retry SQL execution to hide the error.

An old Bind on a different backend, or after server-cache eviction, requires a
new internal Parse. Its descriptor is then rebuilt against that backend's
current schema. Transaction pooling cannot preserve a descriptor held only in
a PostgreSQL backend that has been retired or whose statement was evicted.

## Reuse and performance

Repeated `Bind`/`Execute` calls without a new Parse reuse the same logical
statement. If its internal name is already present on the selected backend,
PostgreSQL can reuse its prepared plan. Otherwise PgDoorman prepares it there
before forwarding the operation, preserving the frontend response order.

A driver sending a new anonymous Parse on every call pays PostgreSQL's prepare
cost on every call. PgDoorman no longer synthesizes ParseComplete to reuse a
shared physical plan across fresh Parses or different clients. Prefer the
driver's persistent named-statement API for repeated queries when appropriate
for the application. Measure the effect with the actual driver and workload;
cache size cannot restore the removed Parse-skipping behavior.

The pool still shares immutable `Arc<Parse>` metadata, SQL text and parameter
OID arrays across clients. Each logical statement owns only its separate
backend alias and client bookkeeping. Migration reconstructs separate aliases
while retaining shared metadata.

```text
Client A: Parse("old", SQL)   -> PostgreSQL: Parse("DOORMAN_42", SQL)
Client B: Parse("new", SQL)   -> PostgreSQL: Parse("DOORMAN_43", SQL)
Client A: Bind("old")         -> PostgreSQL: Bind("DOORMAN_42")
Client B: Bind("new")         -> PostgreSQL: Bind("DOORMAN_43")
```

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

Replacing a client entry schedules closure of its previous backend name after
the new Parse succeeds. A failed Parse restores the previous client namespace.
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
and the prepared-statements web views describe shared pool entries; their
canonical metadata names need not be physical names on a backend.

The pool prepared-entry hit/miss counters describe Parse-time physical reuse.
Fresh Parses now record misses. Backend cache hits still measure reuse by later
Bind/Describe operations. Do not interpret a zero pool Parse-hit rate as a lack
of prepared-plan reuse by persistent named statements.

Use `pg_prepared_statements` on the backend being inspected to see its actual
physical names and plan counts. The client Anonymous and Named eviction metrics
measure their respective capacity pressure; repeated replacement of the same
anonymous entry does not increment the Named eviction counter.

## Reference

- [Pool Modes](../concepts/pool-modes.md)
- [General Settings](../reference/general.md)
- [Admin Commands](../observability/admin-commands.md)
- [Prometheus](../reference/prometheus.md)
- [Query interner monitoring](../operations/monitoring-interner.md)
