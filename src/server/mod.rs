//! `crate::server` module (backend PostgreSQL connection and protocol handling).

pub(crate) mod authentication;
pub(crate) mod cleanup;
pub(crate) mod parameters;
pub(crate) mod prepared_statements;
pub(crate) mod protocol_io;
pub(crate) mod startup_cancel;
pub(crate) mod startup_error;
pub(crate) mod stream;

mod prepared_statement_cache;
mod server_backend;

pub use parameters::ServerParameters;
pub use prepared_statement_cache::{
    anon_len, anon_snapshot, anon_stats, gc_sweep_anon, gc_sweep_named, intern_query, named_len,
    named_snapshot, named_stats, now_monotonic_ms, record_query_count, record_query_duration_us,
    reset_interners_force, set_interner_worker_threads, AnonEntry, CacheEntryKind, GcStats,
    NamedEntry, PreparedStatementCache, QueryInternerKindStats,
};

#[cfg(test)]
pub use prepared_statement_cache::{
    anon_entry_for_test, named_entry_for_test, reset_interners_for_test,
};
/// re-export the graceful-Terminate drain helper used by
/// `app::server::binary_upgrade_and_shutdown`.
pub use server_backend::wait_terminate_tasks_drained;
pub use server_backend::Server;
/// exported so client-side BufReader sites can mirror the
/// backend `BufStream` capacity without duplicating the constant.
pub use server_backend::BUF_STREAM_CAPACITY;
/// re-exported so the client checkout path can dispatch on the
/// parameter-sync classifier (`Server::compute_sync_plan`) without reaching
/// into the private `server_backend` module. `SyncPlan` is `pub(crate)`, so
/// the re-export matches that visibility (a `pub use` would be E0365).
pub(crate) use server_backend::{AsyncExpectedResponse, SyncPlan, HOUSEKEEPING_TIMEOUT};
pub use stream::StreamInner;
