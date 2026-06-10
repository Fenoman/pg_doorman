mod batch_handling;
pub mod buffer_pool;
mod core;
mod entrypoint;
mod error_handling;
#[cfg(unix)]
pub mod migration;
mod protocol;
mod startup;
mod transaction;
pub(crate) mod util;

pub use core::Client;
// re-exported for `benches/prepared_cache_memory_benchmarks.rs` and
// any future bench/test that needs to seed a cache and compare the O(N)
// walk against the O(1) incremental counter introduced by the PR.
pub use core::{
    CachedStatement, PreparedStatementCache, PreparedStatementKey, PreparedStatementKeyRef,
    PutOutcome,
};
pub use entrypoint::{
    client_entrypoint, client_entrypoint_too_many_clients_already,
    client_entrypoint_too_many_clients_already_unix, client_entrypoint_unix, ClientSessionInfo,
};
pub use startup::startup_tls;
pub use util::PREPARED_STATEMENT_COUNTER;
