//! Auth query executor and cache for fetching credentials from PostgreSQL.
//!
//! Two main components:
//! - `AuthQueryExecutor`: manages a small pool of persistent connections via
//!   an mpsc channel and executes parameterized SELECT queries.
//! - `AuthQueryCache`: per-pool credential cache with double-checked locking,
//!   TTL-based expiration, negative caching, and rate-limited re-fetch.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use futures::TryStreamExt;
use log::{debug, error, info, warn};
use smallvec;

use crate::utils::format_elapsed;
use tokio::sync::Mutex as TokioMutex;
use tokio_postgres::types::{FromSql, Type};
use tokio_postgres::{Client, NoTls};

use crate::config::{AuthQueryConfig, Duration, MAX_AUTH_QUERY_WORKERS};
use crate::errors::Error;
use crate::stats::auth_query::AuthQueryStats;

/// Maximum username length accepted by the cache.
/// PostgreSQL limits role names to NAMEDATALEN - 1 = 63 bytes.
/// Usernames exceeding this are rejected without caching to prevent
/// memory exhaustion from very long usernames.
const MAX_USERNAME_LEN: usize = 63;
const MAX_AUTH_QUERY_VERIFIER_LEN: usize = 4096;
const AUTH_QUERY_RECONNECT_INITIAL_BACKOFF_MS: u64 = 100;
const AUTH_QUERY_RECONNECT_MAX_BACKOFF_MS: u64 = 5_000;

/// Marker string the custom `LimitedJson` decoder embeds in its error
/// message when a `json`/`jsonb` row exceeds `MAX_OPERATOR_BUDGET`. The
/// outer `try_get` error has to stay generic (`Box<dyn Error>`), so the
/// auth_query reader matches on this substring to decide between
/// `auth_query_oversize` (operator footgun) and `auth_query_bad_type`
/// (decoder failure on unexpected wire bytes).
const LIMITED_JSON_OVERSIZE_TAG: &str = "auth_query startup_parameters oversize";
const LIMITED_TEXT_OVERSIZE_TAG: &str = "auth_query startup_parameters text oversize";

/// Custom `FromSql` wrapper for `json`/`jsonb` columns that enforces
/// `MAX_OPERATOR_BUDGET` on the raw wire bytes BEFORE `serde_json` walks
/// the value tree. Without this, a malicious or accidentally large
/// `jsonb` row on the auth_query path would force pg_doorman to
/// allocate and parse the full tree on every cache miss, even though
/// the result is later rejected by the size gate inside
/// `parse_startup_parameters_value`. The text decoder already has this
/// pre-parse gate; this wrapper closes the asymmetry for jsonb.
struct LimitedJson(serde_json::Value);

impl<'a> FromSql<'a> for LimitedJson {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let budget = crate::config::startup_parameters::MAX_OPERATOR_BUDGET;
        if raw.len() > budget {
            return Err(format!(
                "{LIMITED_JSON_OVERSIZE_TAG}: raw column is {} bytes, exceeding operator budget {budget}",
                raw.len()
            )
            .into());
        }
        let value = <serde_json::Value as FromSql>::from_sql(ty, raw)?;
        Ok(LimitedJson(value))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::JSON | Type::JSONB)
    }
}

/// Custom `FromSql` wrapper for text-like startup_parameters columns.
/// It checks the raw column bytes before `tokio_postgres` allocates a
/// `String`, matching the json/jsonb `LimitedJson` guard.
#[derive(Debug)]
struct LimitedText(String);

impl<'a> FromSql<'a> for LimitedText {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let budget = crate::config::startup_parameters::MAX_OPERATOR_BUDGET;
        if raw.len() > budget {
            return Err(format!(
                "{LIMITED_TEXT_OVERSIZE_TAG}: raw column is {} bytes, exceeding operator budget {budget}",
                raw.len()
            )
            .into());
        }
        let value = <String as FromSql>::from_sql(ty, raw)?;
        Ok(LimitedText(value))
    }

    fn accepts(ty: &Type) -> bool {
        <String as FromSql>::accepts(ty)
    }
}

struct LimitedVerifier(String);

impl<'a> FromSql<'a> for LimitedVerifier {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() > MAX_AUTH_QUERY_VERIFIER_LEN {
            return Err(format!(
                "auth_query password verifier is {} bytes, exceeds max {MAX_AUTH_QUERY_VERIFIER_LEN}",
                raw.len()
            )
            .into());
        }
        let value = <String as FromSql>::from_sql(ty, raw)?;
        Ok(LimitedVerifier(value))
    }

    fn accepts(ty: &Type) -> bool {
        <String as FromSql>::accepts(ty)
    }
}

fn validate_auth_query_verifier_len(
    password_hash: &str,
    username: &str,
    pool_name: &str,
) -> Result<(), Error> {
    if password_hash.len() > MAX_AUTH_QUERY_VERIFIER_LEN {
        return Err(Error::AuthQueryConfigError(format!(
            "[{username}@{pool_name}] auth_query password verifier is {} bytes, exceeds max {MAX_AUTH_QUERY_VERIFIER_LEN}",
            password_hash.len()
        )));
    }
    Ok(())
}

fn auth_query_reconnect_backoff(attempt: u32) -> std::time::Duration {
    let shift = attempt.saturating_sub(1).min(5);
    let millis = AUTH_QUERY_RECONNECT_INITIAL_BACKOFF_MS
        .saturating_mul(1u64 << shift)
        .min(AUTH_QUERY_RECONNECT_MAX_BACKOFF_MS);
    std::time::Duration::from_millis(millis)
}

// ---------------------------------------------------------------------------
// PasswordFetcher trait (allows mocking AuthQueryExecutor in unit tests)
// ---------------------------------------------------------------------------

/// Password hash plus the per-user startup_parameters map returned by
/// auth_query. The map is empty when the optional column is absent, NULL,
/// empty, or fully rejected by validation.
pub type Credentials = (String, std::collections::HashMap<String, String>);

/// Trait for fetching credentials from PostgreSQL.
/// `AuthQueryExecutor` implements this; tests and benchmarks can substitute a mock.
///
/// `fetch` returns the password hash. `fetch_credentials` also returns the
/// optional per-user startup parameter map. Fetchers that do not support that
/// column use the default empty map.
pub trait PasswordFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        username: &'a str,
    ) -> impl Future<Output = Result<Option<String>, Error>> + Send + 'a;

    fn fetch_credentials<'a>(
        &'a self,
        username: &'a str,
    ) -> impl Future<Output = Result<Option<Credentials>, Error>> + Send + 'a {
        async move {
            Ok(self
                .fetch(username)
                .await?
                .map(|p| (p, std::collections::HashMap::new())))
        }
    }
}

impl PasswordFetcher for AuthQueryExecutor {
    fn fetch<'a>(
        &'a self,
        username: &'a str,
    ) -> impl Future<Output = Result<Option<String>, Error>> + Send + 'a {
        self.fetch_password(username)
    }

    fn fetch_credentials<'a>(
        &'a self,
        username: &'a str,
    ) -> impl Future<Output = Result<Option<Credentials>, Error>> + Send + 'a {
        AuthQueryExecutor::fetch_credentials(self, username)
    }
}

// ---------------------------------------------------------------------------
// AuthQueryExecutor
// ---------------------------------------------------------------------------

/// snapshot of the data the reconnect task needs so the
/// `ReturnGuard::Drop` path can launch recovery from a tokio task
/// without re-borrowing `&AuthQueryExecutor`.
#[derive(Clone)]
struct ReconnectData {
    config: AuthQueryConfig,
    server_host: String,
    server_port: u16,
}

/// RAII guard that keeps the auth_query worker slot replenished even
/// if the surrounding task is cancelled mid-query. Without this the
/// `fetch_credentials` cancel path was a connection leak - N cancellations
/// emptied the executor pool and froze every subsequent login on the pool.
///
/// Normal flow: `fetch_credentials` takes ownership back via
/// `guard.client.take()`, leaving `Option::None`, so Drop is a no-op.
/// Cancel flow: `client.is_some()` -> discard the ambiguous Client and spawn a
/// task that reconnects with a fresh executor connection.
struct ReturnGuard {
    client: Option<Client>,
    tx: async_channel::Sender<Client>,
    pool_name: String,
    reconnect_data: ReconnectData,
}

impl Drop for ReturnGuard {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let tx = self.tx.clone();
        let pool_name = self.pool_name.clone();
        let reconnect_data = self.reconnect_data.clone();
        // Schedule the recovery off-Drop. A cancelled future may have been
        // inside `query_raw` or a RowStream poll, so a non-closed Client can
        // still be protocol-busy. Drop it and replenish with a fresh executor
        // connection instead of returning ambiguous state to the pool.
        drop(client);
        AuthQueryExecutor::schedule_reconnect_with(
            tx,
            pool_name,
            reconnect_data,
            "cancelled fetch discarded executor connection",
        );
    }
}

/// Executor for running auth_query SELECT statements against PostgreSQL.
///
/// Uses an mpsc channel as a simple connection pool: `fetch_password()` takes
/// a Client from the channel, executes the query, and returns it back.
/// If all connections are busy, callers wait on the channel.
///
/// used a `tokio::sync::Mutex<mpsc::Receiver<Client>>`. Tokio's
/// `mpsc::Receiver` is single-consumer, so the mutex was needed to share
/// access across the configured `workers` count - but it also serialised
/// every `fetch_credentials` call through one global lock per executor,
/// turning `workers > 1` into a no-op for parallelism. `async-channel` is
/// MPMC so the receiver is `Send + Sync` and can be cloned to recv()
/// concurrently from multiple callers; the channel's internal logic
/// orders waiters fairly without an external mutex.
pub struct AuthQueryExecutor {
    config: AuthQueryConfig,
    pool_name: String,
    server_host: String,
    server_port: u16,
    tx: async_channel::Sender<Client>,
    rx: async_channel::Receiver<Client>,
}

impl AuthQueryExecutor {
    /// Create executor and establish connections eagerly.
    /// All connections MUST succeed before accepting client traffic
    /// (prevents max_connections deadlock).
    pub async fn new(
        config: &AuthQueryConfig,
        pool_name: &str,
        server_host: &str,
        server_port: u16,
    ) -> Result<Self, Error> {
        if config.workers == 0 {
            return Err(Error::AuthQueryConfigError(
                "auth_query.workers must be > 0".into(),
            ));
        }
        if config.workers > MAX_AUTH_QUERY_WORKERS {
            return Err(Error::AuthQueryConfigError(format!(
                "auth_query.workers must be <= {MAX_AUTH_QUERY_WORKERS}"
            )));
        }

        let database = config
            .database
            .clone()
            .unwrap_or_else(|| pool_name.to_string());

        let pg_config = Self::build_pg_config(config, server_host, server_port, &database);

        // async-channel bounded MPMC. Capacity matches workers as
        // before; semantics: producer side blocks when full (same as
        // tokio mpsc), consumer side recv() may run concurrently from
        // many callers (unlike tokio mpsc which required the Mutex).
        let (tx, rx) = async_channel::bounded::<Client>(config.workers as usize);

        for i in 0..config.workers {
            info!(
                "[pool: {pool_name}] auth_query: opening executor connection {}/{} \
                 to {server_host}:{server_port}/{database} as '{}'",
                i + 1,
                config.workers,
                config.user
            );
            let client = Self::connect(
                &pg_config,
                i,
                pool_name,
                server_host,
                server_port,
                &database,
                &config.user,
            )
            .await?;
            tx.send(client).await.map_err(|_| {
                Error::AuthQueryConnectionError(
                    "failed to initialize executor pool: channel closed".into(),
                )
            })?;
        }

        info!(
            "[pool: {pool_name}] auth_query executor ready: \
             {}@{server_host}:{server_port}/{database} (workers={})",
            config.user, config.workers
        );

        Ok(Self {
            config: config.clone(),
            pool_name: pool_name.to_string(),
            server_host: server_host.to_string(),
            server_port,
            tx,
            rx,
        })
    }

    fn build_pg_config(
        config: &AuthQueryConfig,
        server_host: &str,
        server_port: u16,
        database: &str,
    ) -> tokio_postgres::Config {
        let mut pg_config = tokio_postgres::Config::new();
        pg_config.host(server_host);
        pg_config.port(server_port);
        pg_config.user(&config.user);
        if !config.password.is_empty() {
            pg_config.password(&config.password);
        }
        pg_config.dbname(database);
        pg_config.connect_timeout(std::time::Duration::from_secs(5));
        pg_config
    }

    fn reconnect_data(&self) -> ReconnectData {
        ReconnectData {
            config: self.config.clone(),
            server_host: self.server_host.clone(),
            server_port: self.server_port,
        }
    }

    fn schedule_reconnect(&self, reason: &'static str) {
        Self::schedule_reconnect_with(
            self.tx.clone(),
            self.pool_name.clone(),
            self.reconnect_data(),
            reason,
        );
    }

    fn schedule_reconnect_with(
        tx: async_channel::Sender<Client>,
        pool_name: String,
        reconnect_data: ReconnectData,
        reason: &'static str,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            error!(
                "[pool: {pool_name}] auth_query: cannot schedule reconnect after {reason}: \
                 no Tokio runtime"
            );
            return;
        };
        handle.spawn(async move {
            let database = reconnect_data
                .config
                .database
                .clone()
                .unwrap_or_else(|| pool_name.clone());
            let pg_config = AuthQueryExecutor::build_pg_config(
                &reconnect_data.config,
                &reconnect_data.server_host,
                reconnect_data.server_port,
                &database,
            );
            let mut attempt = 1u32;
            loop {
                if tx.is_closed() {
                    warn!(
                        "[pool: {pool_name}] auth_query: reconnect stopped after {reason}: \
                         executor channel closed"
                    );
                    return;
                }

                warn!("[pool: {pool_name}] auth_query: {reason}, reconnect attempt {attempt}");
                match AuthQueryExecutor::connect(
                    &pg_config,
                    attempt,
                    &pool_name,
                    &reconnect_data.server_host,
                    reconnect_data.server_port,
                    &database,
                    &reconnect_data.config.user,
                )
                .await
                {
                    Ok(new_client) => match tx.try_send(new_client) {
                        Ok(()) => return,
                        Err(async_channel::TrySendError::Full(_client)) => {
                            warn!(
                                "[pool: {pool_name}] auth_query: reconnect produced a Client \
                                 but executor channel is already full; dropping duplicate"
                            );
                            return;
                        }
                        Err(async_channel::TrySendError::Closed(_client)) => {
                            warn!(
                                "[pool: {pool_name}] auth_query: reconnect produced a Client \
                                 but executor channel closed"
                            );
                            return;
                        }
                    },
                    Err(connect_err) => {
                        let backoff = auth_query_reconnect_backoff(attempt);
                        error!(
                            "[pool: {pool_name}] auth_query: reconnect attempt {attempt} failed: \
                             {connect_err}; retrying in {backoff:?}"
                        );
                        attempt = attempt.saturating_add(1);
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });
    }

    async fn connect(
        pg_config: &tokio_postgres::Config,
        index: u32,
        pool_name: &str,
        server_host: &str,
        server_port: u16,
        database: &str,
        user: &str,
    ) -> Result<Client, Error> {
        let start = std::time::Instant::now();
        let (client, connection) = pg_config.connect(NoTls).await.map_err(|e| {
            error!(
                "[pool: {pool_name}] auth_query: executor connection {index} failed to \
                 {server_host}:{server_port}/{database} as '{user}': {e}"
            );
            Error::AuthQueryConnectionError(format!(
                "connection {index} to {server_host}:{server_port}/{database} as '{user}': {e}"
            ))
        })?;
        let elapsed = format_elapsed(start.elapsed());

        info!(
            "[pool: {pool_name}] auth_query: executor connection {index} established \
             to {server_host}:{server_port}/{database} as '{user}' ({elapsed})"
        );

        let pool_name_owned = pool_name.to_string();
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                error!(
                    "[pool: {pool_name_owned}] auth_query executor connection {index} lost: {e}"
                );
            }
        });

        Ok(client)
    }

    /// Fetch credentials (password hash plus the optional per-user
    /// startup_parameters map) for a username from PostgreSQL.
    /// Returns `Some((password_hash, params))` or `None` if user not found.
    pub async fn fetch_credentials(&self, username: &str) -> Result<Option<Credentials>, Error> {
        debug!(
            "[{username}@{}] auth_query: fetching credentials",
            self.pool_name
        );

        // shared async-channel receiver - multiple in-flight
        // calls recv concurrently without an external mutex. The previous
        // shape had a critical availability bug: if the caller's task was
        // cancelled (client disconnect, login timeout, RELOAD drain) AFTER
        // `rx.recv()` returned but BEFORE `tx.send()` ran, the Client was
        // dropped instead of returned to the channel. After N such
        // cancellations the executor pool was empty and every subsequent
        // login on this pool hung forever on `rx.recv()`. RAII via
        // `ReturnGuard` is the fix - `Drop` runs on cancellation too, and
        // synchronously dispatches the Client back to the channel (or
        // schedules a reconnect on a closed connection).
        let client = self.rx.recv().await.map_err(|_| {
            error!(
                "[{username}@{}] auth_query: executor pool closed, cannot fetch credentials",
                self.pool_name
            );
            Error::AuthQueryPoolClosed
        })?;

        // Move ownership into a guard. On normal completion we extract the
        // Client back via `into_inner()`. On `await`-point cancellation the
        // guard's Drop discards ambiguous protocol state and triggers reconnect.
        let mut guard = ReturnGuard {
            client: Some(client),
            tx: self.tx.clone(),
            pool_name: self.pool_name.clone(),
            // Snapshot of config so reconnect can run from Drop without
            // borrowing &self.
            reconnect_data: ReconnectData {
                config: self.config.clone(),
                server_host: self.server_host.clone(),
                server_port: self.server_port,
            },
        };

        let start = std::time::Instant::now();
        let result = self
            .execute_query(guard.client.as_ref().unwrap(), username)
            .await;
        let elapsed = format_elapsed(start.elapsed());

        match &result {
            Ok(Some(_)) => {
                debug!(
                    "[{username}@{}] auth_query: password found ({elapsed})",
                    self.pool_name
                );
            }
            Ok(None) => {
                debug!(
                    "[{username}@{}] auth_query: user not found ({elapsed})",
                    self.pool_name
                );
            }
            Err(e) => {
                error!(
                    "[{username}@{}] auth_query: query failed ({elapsed}): {e}",
                    self.pool_name
                );
            }
        }

        // took client out of the guard BEFORE
        // `tx.send().await` - but `send().await` is NOT cancel-safe in
        // async-channel; a caller cancelled at that await point would
        // drop the Client and leak the executor slot, reintroducing
        // the exact bug F3 fixed at the recv-side. The fix: use the
        // sync `try_send` while we still hold the guard. The channel
        // capacity matches workers and the slot we recv'd from is
        // immediately available, so `try_send` returns Ok in the happy
        // path. If it returns Err (channel closed / full from races),
        // the guard's Drop catches the cleanup.
        if result.is_ok() {
            let client = guard.client.take().expect("guard still holds Client");
            if !client.is_closed() {
                if let Err(send_err) = self.tx.try_send(client) {
                    warn!(
                        "[{username}@{}] auth_query: try_send back to channel failed: {send_err}",
                        self.pool_name
                    );
                    // try_send took ownership of `client` on Err but
                    // exposes it via `into_inner()`; the guard already
                    // took() so we lose the slot here. Trigger reconnect
                    // from an owned task to repopulate.
                    drop(send_err);
                    self.schedule_reconnect("try_send back to channel failed");
                }
            } else {
                warn!(
                    "[{username}@{}] auth_query: executor connection dead after successful query, \
                     attempting reconnect",
                    self.pool_name
                );
                drop(client);
                self.schedule_reconnect("executor connection dead after successful query");
            }
        } else {
            // On query failure: discard the client and trigger reconnect.
            // Guard.take() drops the client without sending; the guard's
            // Drop is now a no-op for the `result is None` case.
            let _ = guard.client.take();
            warn!(
                "[{username}@{}] auth_query: executor connection dead after query failure, \
                 attempting reconnect",
                self.pool_name
            );
            self.schedule_reconnect("executor connection dead after query failure");
        }

        result
    }

    /// Backwards-compatible password-only accessor. Discards any per-user
    /// startup_parameters returned alongside the password.
    pub async fn fetch_password(&self, username: &str) -> Result<Option<String>, Error> {
        Ok(self.fetch_credentials(username).await?.map(|(p, _)| p))
    }

    async fn execute_query(
        &self,
        client: &Client,
        username: &str,
    ) -> Result<Option<Credentials>, Error> {
        let rows = client
            .query_raw(
                &self.config.query,
                [&username as &(dyn tokio_postgres::types::ToSql + Sync)],
            )
            .await
            .map_err(|e| {
                Error::AuthQueryQueryError(format!(
                    "query execution failed for user '{username}': {e}"
                ))
            })?;
        futures::pin_mut!(rows);

        let Some(row) = rows.try_next().await.map_err(|e| {
            Error::AuthQueryQueryError(format!("query execution failed for user '{username}': {e}"))
        })?
        else {
            return Ok(None);
        };

        if rows
            .try_next()
            .await
            .map_err(|e| {
                Error::AuthQueryQueryError(format!(
                    "query execution failed for user '{username}': {e}"
                ))
            })?
            .is_some()
        {
            return Err(Error::AuthQueryConfigError(format!(
                "query returned more than 1 row for user '{username}', expected 0 or 1"
            )));
        }

        let pw_opt = Self::extract_password(&row, username, &self.pool_name)?;
        let Some(pw) = pw_opt else {
            return Ok(None);
        };
        let params = Self::extract_startup_parameters(&row, username, &self.pool_name);
        Ok(Some((pw, params)))
    }

    /// Extract password hash from query result row.
    ///
    /// Column lookup priority:
    /// 1. Column named `passwd` (matches `pg_shadow.passwd`)
    /// 2. Column named `password`
    /// 3. If the query returns exactly one column, use it regardless of name
    fn extract_password(
        row: &tokio_postgres::Row,
        username: &str,
        pool_name: &str,
    ) -> Result<Option<String>, Error> {
        let columns = row.columns();
        let password: Option<String> = if columns.iter().any(|c| c.name() == "passwd") {
            row.try_get::<_, Option<LimitedVerifier>>("passwd")
                .map_err(|e| {
                    Error::AuthQueryConfigError(format!(
                        "failed to read passwd from auth_query result: {e}"
                    ))
                })?
                .map(|p| p.0)
        } else if columns.iter().any(|c| c.name() == "password") {
            row.try_get::<_, Option<LimitedVerifier>>("password")
                .map_err(|e| {
                    Error::AuthQueryConfigError(format!(
                        "failed to read password from auth_query result: {e}"
                    ))
                })?
                .map(|p| p.0)
        } else if columns.len() == 1 {
            row.try_get::<_, Option<LimitedVerifier>>(0)
                .map_err(|e| {
                    Error::AuthQueryConfigError(format!(
                        "failed to read password from single-column result: {e}"
                    ))
                })?
                .map(|p| p.0)
        } else {
            let col_names: Vec<&str> = columns.iter().map(|c| c.name()).collect();
            return Err(Error::AuthQueryConfigError(format!(
                "cannot find password column for user '{username}': \
                 expected column named 'passwd' or 'password', or a single-column result; \
                 got columns: {col_names:?}"
            )));
        };
        match password {
            Some(p) if !p.is_empty() => {
                validate_auth_query_verifier_len(&p, username, pool_name)?;
                Ok(Some(p))
            }
            _ => {
                warn!("[{username}@{pool_name}] auth_query: password is NULL or empty");
                Ok(None)
            }
        }
    }

    /// Read the optional `startup_parameters` column from the auth_query
    /// row. A missing column yields an empty map. Column type drives the
    /// decoder so a `jsonb` row goes straight through `serde_json::Value`
    /// without a `text`-decode failure or a serialize/re-parse
    /// round-trip; an unsupported column type logs a warning, ticks the
    /// drop counter, and yields an empty map. Actual JSON shape /
    /// per-entry validation is shared via
    /// `parse_startup_parameters_value`.
    fn extract_startup_parameters(
        row: &tokio_postgres::Row,
        username: &str,
        pool_name: &str,
    ) -> std::collections::HashMap<String, String> {
        let column = row
            .columns()
            .iter()
            .find(|c| c.name() == "startup_parameters");
        let Some(column) = column else {
            return std::collections::HashMap::new();
        };
        let col_type = column.type_();
        if matches!(*col_type, Type::JSON | Type::JSONB) {
            // The custom `LimitedJson` wrapper enforces the
            // `MAX_OPERATOR_BUDGET` cap on the raw wire bytes from PG
            // before `serde_json` walks the tree. Without this gate the
            // text path (which checks `text.len()` before
            // `from_str`) and the json/jsonb path diverge: a malicious
            // or accidentally oversize `jsonb` row would still force
            // pg_doorman to materialise the full `Value` tree before
            // discarding it.
            match row.try_get::<_, Option<LimitedJson>>("startup_parameters") {
                Ok(Some(LimitedJson(value))) => {
                    Self::parse_startup_parameters_value(value, username, pool_name)
                }
                Ok(None) => std::collections::HashMap::new(),
                Err(json_err) => {
                    if json_err.to_string().contains(LIMITED_JSON_OVERSIZE_TAG) {
                        warn!(
                            "[{username}@{pool_name}] auth_query startup_parameters: {json_err}; \
                             parameters ignored"
                        );
                        crate::web::metrics::observe_startup_parameters_dropped(
                            pool_name,
                            "auth_query_oversize",
                        );
                    } else {
                        warn!(
                            "[{username}@{pool_name}] auth_query startup_parameters column has type \
                             `{ty}` but pg_doorman could not decode it as json: {json_err}. \
                             Per-user parameters are ignored for this row.",
                            ty = col_type.name()
                        );
                        crate::web::metrics::observe_startup_parameters_dropped(
                            pool_name,
                            "auth_query_bad_type",
                        );
                    }
                    std::collections::HashMap::new()
                }
            }
        } else {
            match row.try_get::<_, Option<LimitedText>>("startup_parameters") {
                Ok(raw) => Self::parse_startup_parameters_text(
                    raw.as_ref().map(|text| text.0.as_str()),
                    username,
                    pool_name,
                ),
                Err(text_err) => {
                    if text_err.to_string().contains(LIMITED_TEXT_OVERSIZE_TAG) {
                        warn!(
                            "[{username}@{pool_name}] auth_query startup_parameters: {text_err}; \
                             parameters ignored"
                        );
                        crate::web::metrics::observe_startup_parameters_dropped(
                            pool_name,
                            "auth_query_oversize",
                        );
                    } else {
                        warn!(
                            "[{username}@{pool_name}] auth_query startup_parameters column has type \
                             `{ty}` but pg_doorman reads it as `text`, `json`, or `jsonb`: {text_err}. \
                             Per-user parameters are ignored for this row.",
                            ty = col_type.name()
                        );
                        crate::web::metrics::observe_startup_parameters_dropped(
                            pool_name,
                            "auth_query_bad_type",
                        );
                    }
                    std::collections::HashMap::new()
                }
            }
        }
    }

    /// Parse the optional `startup_parameters` JSON object received as
    /// `text` from auth_query. Wraps `parse_startup_parameters_value`
    /// after the size gate and `serde_json::from_str`.
    fn parse_startup_parameters_text(
        text: Option<&str>,
        username: &str,
        pool_name: &str,
    ) -> std::collections::HashMap<String, String> {
        let Some(text) = text else {
            return std::collections::HashMap::new();
        };
        if text.is_empty() {
            return std::collections::HashMap::new();
        }
        // Reject oversize input before serde_json allocates the Value tree.
        // A single auth_query row above the operator budget cannot produce a
        // sendable startup map.
        let max_bytes = crate::config::startup_parameters::MAX_OPERATOR_BUDGET;
        if text.len() > max_bytes {
            warn!(
                "[{username}@{pool_name}] auth_query startup_parameters: raw column is {} bytes, \
                 exceeding operator budget {max_bytes}; parameters ignored",
                text.len()
            );
            crate::web::metrics::observe_startup_parameters_dropped(
                pool_name,
                "auth_query_oversize",
            );
            return std::collections::HashMap::new();
        }
        let parsed: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[{username}@{pool_name}] auth_query startup_parameters: JSON parse failed: \
                     {e}; parameters ignored"
                );
                crate::web::metrics::observe_startup_parameters_dropped(
                    pool_name,
                    "auth_query_invalid_json",
                );
                return std::collections::HashMap::new();
            }
        };
        Self::parse_startup_parameters_value(parsed, username, pool_name)
    }

    /// Per-entry validation shared between the `text` and `json`/`jsonb`
    /// auth_query decoders. The json/jsonb path used to serialise the
    /// `serde_json::Value` back into a string and re-parse it here; this
    /// helper avoids the round-trip.
    fn parse_startup_parameters_value(
        value: serde_json::Value,
        username: &str,
        pool_name: &str,
    ) -> std::collections::HashMap<String, String> {
        let serde_json::Value::Object(obj) = value else {
            warn!(
                "[{username}@{pool_name}] auth_query startup_parameters: top-level value is not a \
                 JSON object; ignored"
            );
            crate::web::metrics::observe_startup_parameters_dropped(
                pool_name,
                "auth_query_invalid_shape",
            );
            return std::collections::HashMap::new();
        };
        let mut out = std::collections::HashMap::with_capacity(obj.len());
        let scope = format!("auth_query.startup_parameters[user={username}]");
        let mut had_invalid_entry = false;
        for (k, v) in obj {
            match v {
                serde_json::Value::String(s) => {
                    if let Err(e) =
                        crate::config::startup_parameters::validate_entry(&k, &s, &scope)
                    {
                        warn!("[{pool_name}] {e}");
                        had_invalid_entry = true;
                        continue;
                    }
                    // Canonicalise tracked GUC names so the per-user
                    // overlay merges with the general/pool cascade by
                    // canonical key. Without this, an auth_query row
                    // returning `timezone` would not override a pool
                    // `TimeZone` baseline and both would survive.
                    let canonical = crate::server::parameters::canonicalize_param_name(k);
                    out.insert(canonical, s);
                }
                other => {
                    let kind = match other {
                        serde_json::Value::Null => "null",
                        serde_json::Value::Bool(_) => "boolean",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::Object(_) => "object",
                        serde_json::Value::String(_) => unreachable!(),
                    };
                    warn!(
                        "[{username}@{pool_name}] auth_query startup_parameters: value for '{k}' \
                         is {kind}, not string; ignored"
                    );
                    had_invalid_entry = true;
                }
            }
        }
        // One increment per parsed row that contained at least one
        // invalid entry, matching every other reason on this counter
        // so `rate by(reason)` is dimensionally consistent. Per-entry
        // detail stays in the warn log.
        if had_invalid_entry {
            crate::web::metrics::observe_startup_parameters_dropped(
                pool_name,
                "auth_query_invalid_entry",
            );
        }
        // The text decoder enforces `MAX_OPERATOR_BUDGET` on the raw
        // column before parsing. The json/jsonb decoder has no raw
        // bytes to size, so apply the same gate to the wire-shape of
        // the resulting map. Without this a large `jsonb` row would be
        // cached and then rejected at backend startup with a less
        // helpful 53400; here we drop it as `auth_query_oversize` like
        // the text path.
        let max_bytes = crate::config::startup_parameters::MAX_OPERATOR_BUDGET;
        let serialized = out
            .iter()
            .map(|(k, v)| k.len() + 1 + v.len() + 1)
            .sum::<usize>();
        if serialized > max_bytes {
            warn!(
                "[{username}@{pool_name}] auth_query startup_parameters: decoded map is \
                 {serialized} bytes, exceeding operator budget {max_bytes}; parameters ignored"
            );
            crate::web::metrics::observe_startup_parameters_dropped(
                pool_name,
                "auth_query_oversize",
            );
            return std::collections::HashMap::new();
        }
        out
    }
}

// ---------------------------------------------------------------------------
// CacheEntry
// ---------------------------------------------------------------------------

/// Immutable snapshot of a user's auth_query overlay: the wire map plus
/// the precomputed hash the dynamic-pool fast path consumes. The two
/// values move together by contract — direct field writes risk drift
/// (e.g. swapping the map without touching the hash), so the struct
/// only exposes accessors and constructors. Cheap to clone because the
/// inner `Arc<HashMap>` is shared.
#[derive(Clone, Debug)]
pub struct StartupOverlay {
    map: Arc<std::collections::HashMap<String, String>>,
    hash: u64,
}

impl StartupOverlay {
    pub fn empty() -> Self {
        Self {
            map: Arc::new(std::collections::HashMap::new()),
            hash: crate::pool::empty_overlay_hash(),
        }
    }

    pub fn from_map(map: std::collections::HashMap<String, String>) -> Self {
        let hash = crate::pool::per_user_overlay_hash(map.iter());
        Self {
            map: Arc::new(map),
            hash,
        }
    }

    pub fn map(&self) -> &Arc<std::collections::HashMap<String, String>> {
        &self.map
    }

    pub fn hash(&self) -> u64 {
        self.hash
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Single cache entry for a username's credentials.
#[derive(Clone, Debug)]
pub struct CacheEntry {
    /// Password hash from pg_shadow ("md5..." or "SCRAM-SHA-256$...")
    pub password_hash: String,
    /// When this entry was fetched from PG
    pub fetched_at: Instant,
    /// True if user was NOT found in pg_shadow
    pub is_negative: bool,
    /// When was the last re-fetch attempted for this user (rate limiting)
    pub last_refetch_at: Option<Instant>,
    /// SCRAM ClientKey extracted from client's proof (Step 5).
    /// Stored here so pool connections created later can use it
    /// for SCRAM passthrough to backend PG (Step 6).
    /// None for MD5 users or before first SCRAM auth.
    pub client_key: Option<Vec<u8>>,
    /// Per-user startup parameters returned by the optional auth_query
    /// `startup_parameters` JSON column, paired with their overlay hash.
    /// The map is empty (and hash is `empty_overlay_hash()`) when the
    /// column is absent, empty/NULL, or filtered out in dedicated
    /// auth_query mode. `StartupOverlay` keeps map and hash from
    /// drifting — mutate via [`Self::set_startup_overlay`].
    pub startup_overlay: StartupOverlay,
}

impl CacheEntry {
    fn positive(password_hash: String) -> Self {
        Self {
            password_hash,
            fetched_at: Instant::now(),
            is_negative: false,
            last_refetch_at: None,
            client_key: None,
            startup_overlay: StartupOverlay::empty(),
        }
    }

    fn negative() -> Self {
        Self {
            password_hash: String::new(),
            fetched_at: Instant::now(),
            is_negative: true,
            last_refetch_at: None,
            client_key: None,
            startup_overlay: StartupOverlay::empty(),
        }
    }

    /// Replace the overlay with one built from the given map; the new
    /// hash is computed inside the constructor.
    pub fn set_startup_overlay(
        &mut self,
        startup_parameters: std::collections::HashMap<String, String>,
    ) {
        self.startup_overlay = StartupOverlay::from_map(startup_parameters);
    }

    fn is_expired(&self, cache_ttl: &Duration, cache_failure_ttl: &Duration) -> bool {
        let ttl_ms = if self.is_negative {
            cache_failure_ttl.as_millis()
        } else {
            cache_ttl.as_millis()
        };
        self.fetched_at.elapsed().as_millis() as u64 >= ttl_ms
    }
}

// ---------------------------------------------------------------------------
// AuthQueryCache
// ---------------------------------------------------------------------------

/// Per-pool auth query cache with double-checked locking.
///
/// Caches credentials fetched by `AuthQueryExecutor` to avoid hitting PG
/// on every client authentication. Supports:
/// - Positive caching (user found) with `cache_ttl`
/// - Negative caching (user not found) with `cache_failure_ttl`
/// - Per-username locks for request coalescing (double-checked locking)
/// - Rate-limited re-fetch after auth failure (`min_interval`)
///
/// Generic over the fetcher: defaults to `AuthQueryExecutor` in production,
/// tests substitute a mock.
pub struct AuthQueryCache<F = AuthQueryExecutor> {
    /// Pool name for log context.
    pool_name: String,
    /// Cached credentials keyed by username.
    ///
    /// values are `Arc<CacheEntry>` so a cache hit returns a
    /// cheap pointer clone instead of deep-cloning `password_hash`
    /// (String) and `client_key` (Vec<u8>) on every authentication. The
    /// in-place `set_client_key` mutation uses `Arc::make_mut`, which only
    /// copies when another reader is concurrently holding the Arc.
    entries: DashMap<String, Arc<CacheEntry>>,
    /// Per-username locks for request coalescing.
    /// First request acquires lock + fetches; others wait + get cache hit.
    locks: DashMap<String, Arc<TokioMutex<()>>>,
    /// Fetcher for cache miss to PG.
    executor: Arc<F>,
    /// TTL for positive cache entries (user found).
    cache_ttl: Duration,
    /// TTL for negative cache entries (user not found).
    cache_failure_ttl: Duration,
    /// Minimum interval between re-fetches (rate limiting).
    min_interval: Duration,
    /// Optional stats for observability (None in unit tests).
    stats: Option<Arc<AuthQueryStats>>,
    /// True when auth_query runs in dedicated mode (server_user is set).
    /// In that mode every backend connection shares a single backend
    /// identity, so per-user startup_parameters cannot be honored.
    is_dedicated: bool,
    /// Usernames already warned about dropped per-user startup_parameters
    /// in dedicated mode. Ensures the warning fires at most once per
    /// (pool, user) until the cache is cleared by a config reload.
    dedicated_warnings: DashMap<String, ()>,
    /// counter for amortized sampled TTL eviction.
    /// Every `SWEEP_INTERVAL` calls to `get_or_fetch` walk
    /// `SWEEP_BATCH` cache entries and drop the expired ones (plus
    /// their `locks` and `dedicated_warnings` peers). Without this,
    /// `entries`/`locks`/`dedicated_warnings` grew unbounded by unique
    /// username until the next config RELOAD - a brute-force username
    /// probe or a multi-tenant SaaS with short-lived service accounts
    /// could pin tens of megabytes of dead state inside the pooler.
    sweep_counter: AtomicU64,
}

/// number of `get_or_fetch` calls between sweeps. 128 is large
/// enough to keep the sweep amortized to <0.01% of authentication
/// cost yet small enough that a 60s cache_failure_ttl entry never
/// outlives more than ~128 negative-cache hits.
const SWEEP_INTERVAL: u64 = 128;

/// number of entries inspected per sweep. 8 keeps the per-call
/// p99 latency overhead trivial (a few atomic reads + at most 8
/// DashMap removes) while still draining the largest realistic
/// caches (10k entries) in under 1 minute of normal traffic at
/// 10 req/sec - well below typical `cache_ttl` (1 hour).
const SWEEP_BATCH: usize = 8;

/// Hard cap for one pool's auth_query credential cache. TTL sweeps remove
/// expired entries, but an attacker can create fresh unique negative-cache
/// entries faster than sampled sweeps age them out. Keep a bounded per-pool
/// footprint even when every attempted username is different.
const MAX_AUTH_QUERY_CACHE_ENTRIES: usize = 4096;

/// Prune below the hard cap once it is crossed so every post-cap insert does
/// not pay a full DashMap scan. The target remains high enough for normal
/// multi-tenant caches while giving a 256-entry hysteresis window.
const AUTH_QUERY_CACHE_TARGET_ENTRIES: usize = 3840;

struct AuthQueryLockCleanupGuard<'a, F: PasswordFetcher> {
    cache: &'a AuthQueryCache<F>,
    username: &'a str,
}

impl<'a, F: PasswordFetcher> AuthQueryLockCleanupGuard<'a, F> {
    fn new(cache: &'a AuthQueryCache<F>, username: &'a str) -> Self {
        Self { cache, username }
    }
}

impl<F: PasswordFetcher> Drop for AuthQueryLockCleanupGuard<'_, F> {
    fn drop(&mut self) {
        self.cache.remove_lock_without_live_entry(self.username);
    }
}

impl<F: PasswordFetcher> AuthQueryCache<F> {
    pub fn new(
        pool_name: String,
        executor: Arc<F>,
        config: &AuthQueryConfig,
        stats: Option<Arc<AuthQueryStats>>,
    ) -> Self {
        Self {
            pool_name,
            entries: DashMap::new(),
            locks: DashMap::new(),
            executor,
            cache_ttl: config.cache_ttl,
            cache_failure_ttl: config.cache_failure_ttl,
            min_interval: config.min_interval,
            stats,
            is_dedicated: config.is_dedicated_mode(),
            dedicated_warnings: DashMap::new(),
            sweep_counter: AtomicU64::new(0),
        }
    }

    fn remove_cached_username(&self, username: &str) {
        self.entries.remove(username);
        self.locks.remove(username);
        self.dedicated_warnings.remove(username);
    }

    fn has_live_entry(&self, username: &str) -> bool {
        self.entries
            .get(username)
            .is_some_and(|entry| !entry.is_expired(&self.cache_ttl, &self.cache_failure_ttl))
    }

    fn remove_lock_without_live_entry(&self, username: &str) {
        if !self.has_live_entry(username) {
            self.locks.remove(username);
            self.dedicated_warnings.remove(username);
        }
    }

    fn enforce_size_limit(&self) {
        let len = self.entries.len();
        if len <= MAX_AUTH_QUERY_CACHE_ENTRIES {
            return;
        }

        let remove_goal = len.saturating_sub(AUTH_QUERY_CACHE_TARGET_ENTRIES);
        let mut victims: Vec<(String, std::time::Duration)> = Vec::with_capacity(len);
        for entry in self.entries.iter() {
            victims.push((entry.key().clone(), entry.fetched_at.elapsed()));
        }
        victims.sort_by(|a, b| b.1.cmp(&a.1));

        for (username, _) in victims.into_iter().take(remove_goal) {
            self.remove_cached_username(&username);
        }
    }

    /// drop up to `SWEEP_BATCH` expired entries plus
    /// their `locks` and `dedicated_warnings` peers. Called from the
    /// `get_or_fetch` slow path, amortized to one sweep per
    /// `SWEEP_INTERVAL` calls. Sampled, not exhaustive - slow drift to
    /// steady-state mirrors the prepared-statement cache eviction
    /// policy (sampled K-stride, not full scan).
    fn sweep_expired(&self) {
        let mut to_drop: smallvec::SmallVec<[String; SWEEP_BATCH]> = smallvec::SmallVec::new();
        for entry in self.entries.iter().take(SWEEP_BATCH * 2) {
            if entry.is_expired(&self.cache_ttl, &self.cache_failure_ttl) {
                to_drop.push(entry.key().clone());
                if to_drop.len() >= SWEEP_BATCH {
                    break;
                }
            }
        }
        for key in &to_drop {
            // Dropping `locks[key]` is safe: any awaiter that held the
            // Arc keeps the mutex alive; new requests for the same
            // username re-create the entry under the DashMap shard
            // lock. Doing this in lock-step with `entries.remove`
            // prevents the lock-map from accumulating forever even when
            // no two callers ever race on the same username.
            self.remove_cached_username(key);
        }
    }

    /// In dedicated auth_query mode (`server_user` set) every backend
    /// connection shares a single identity, so per-user startup_parameters
    /// cannot be honored: pg_doorman has no per-user backend on which to
    /// apply them. Drop the parsed map before it reaches downstream code
    /// and warn once per (pool, username) so the operator notices.
    fn dedicated_mode_filter(&self, entry: &mut CacheEntry, username: &str) {
        if !self.is_dedicated || entry.startup_overlay.is_empty() {
            return;
        }
        // One increment per drop event (a single fetched row whose
        // overlay was dropped because of dedicated mode), matching every
        // other reason on this counter so `rate by(reason)` is
        // dimensionally consistent. The warn log carries the same
        // once-per-(pool, user) shape.
        crate::web::metrics::observe_startup_parameters_dropped(
            self.pool_name.as_str(),
            "dedicated_mode",
        );
        if self
            .dedicated_warnings
            .insert(username.to_string(), ())
            .is_none()
        {
            warn!(
                "[{username}@{pool}] per-user startup_parameters ignored in dedicated \
                 auth_query mode; use pool-level startup_parameters instead",
                pool = self.pool_name
            );
        }
        entry.set_startup_overlay(std::collections::HashMap::new());
    }

    /// When a fresh auth_query fetch produces a per-user
    /// `startup_parameters` map that differs from the snapshot frozen
    /// into the live dynamic pool at creation time, drop the pool so
    /// the next client connection rebuilds against the new overlay.
    /// Without this, an operator-side change to the row (`UPDATE
    /// pgbouncer.users SET startup_parameters = ...`) only takes effect
    /// for new dynamic-pool spawns, not for existing pools. Dedicated
    /// mode and the dedicated-mode warning path land here with an empty
    /// map; that compares equal to the empty-overlay hash that
    /// dedicated pools store, so nothing is dropped on that path.
    fn drop_dynamic_pool_if_overlay_drifted(
        &self,
        username: &str,
        new_overlay: &std::collections::HashMap<String, String>,
    ) {
        let identifier = crate::pool::PoolIdentifier::new(&self.pool_name, username);
        if !crate::pool::is_dynamic_pool(&identifier) {
            return;
        }
        let new_hash = crate::pool::per_user_overlay_hash(new_overlay.iter());
        let live_hash = crate::pool::POOLS
            .load()
            .get(&identifier)
            .map(|p| p.per_user_startup_overlay_hash);
        match live_hash {
            Some(h) if h != new_hash => {
                if crate::pool::drop_dynamic_pool(&identifier) {
                    info!(
                        "[{username}@{}] auth_query overlay drift on refetch — dynamic pool dropped, next connect will rebuild",
                        self.pool_name
                    );
                }
            }
            _ => {}
        }
    }

    /// Increment a stats counter if stats are enabled.
    fn inc(&self, counter: fn(&AuthQueryStats) -> &AtomicU64) {
        if let Some(ref stats) = self.stats {
            counter(stats).fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Get password hash for username. Uses cache with double-checked locking.
    ///
    /// Returns:
    /// - `Ok(Some(entry))` — user found (positive cache or fresh fetch)
    /// - `Ok(None)` — user not found (negative cache or fresh fetch returned 0 rows)
    /// - `Err` — executor error (PG down, SQL error, etc.)
    pub async fn get_or_fetch(&self, username: &str) -> Result<Option<Arc<CacheEntry>>, Error> {
        if username.len() > MAX_USERNAME_LEN {
            warn!(
                "[{username}@{}] auth_query cache: rejecting username (len={}, max={MAX_USERNAME_LEN})",
                self.pool_name,
                username.len()
            );
            return Ok(None);
        }

        // amortized sampled TTL eviction. One sweep per
        // SWEEP_INTERVAL calls keeps the cost <0.01% of auth latency
        // while preventing unbounded growth on long-lived pools.
        if self.sweep_counter.fetch_add(1, Ordering::Relaxed) % SWEEP_INTERVAL == 0 {
            self.sweep_expired();
        }

        // Fast path: check cache without lock
        if let Some(entry) = self.entries.get(username) {
            if !entry.is_expired(&self.cache_ttl, &self.cache_failure_ttl) {
                self.inc(|s| &s.cache_hits);
                return if entry.is_negative {
                    Ok(None)
                } else {
                    // cheap Arc pointer clone, not a deep copy.
                    Ok(Some(Arc::clone(entry.value())))
                };
            }
        }

        // Slow path: acquire per-username lock
        let lock = self
            .locks
            .entry(username.to_string())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone();

        let _lock_cleanup = AuthQueryLockCleanupGuard::new(self, username);
        let _guard = lock.lock().await;
        // Keep the guard armed. Normal success publishes a live cache entry,
        // so the guard keeps the coalescing lock. Cancellation or an error
        // before publication drops the lock entry instead of growing the map
        // outside the credential-cache cap.

        // Double-check after acquiring lock
        if let Some(entry) = self.entries.get(username) {
            if !entry.is_expired(&self.cache_ttl, &self.cache_failure_ttl) {
                self.inc(|s| &s.cache_hits);
                return if entry.is_negative {
                    Ok(None)
                } else {
                    // cheap Arc pointer clone, not a deep copy.
                    Ok(Some(Arc::clone(entry.value())))
                };
            }
        }

        // Cache miss: fetch credentials from PG.
        self.inc(|s| &s.executor_queries);
        match self.executor.fetch_credentials(username).await {
            Ok(Some((password_hash, startup_params))) => {
                validate_auth_query_verifier_len(&password_hash, username, &self.pool_name)?;
                self.inc(|s| &s.cache_misses);
                let mut entry = CacheEntry::positive(password_hash);
                entry.set_startup_overlay(startup_params);
                self.dedicated_mode_filter(&mut entry, username);
                // wrap once, store and hand back the same Arc.
                let entry = Arc::new(entry);
                // Publish the fresh entry first so any concurrent
                // create_dynamic_pool peeks the new overlay, then drop the
                // pool whose snapshot drifted. Reversing the order would
                // open a window where the drop runs against the live pool
                // while the cache still holds the old map, and a racing
                // create_dynamic_pool would rebuild against that stale
                // map and immediately drift again.
                self.entries
                    .insert(username.to_string(), Arc::clone(&entry));
                self.enforce_size_limit();
                self.drop_dynamic_pool_if_overlay_drifted(username, entry.startup_overlay.map());
                Ok(Some(entry))
            }
            Ok(None) => {
                self.inc(|s| &s.cache_misses);
                let entry = CacheEntry::negative();
                self.entries.insert(username.to_string(), Arc::new(entry));
                self.enforce_size_limit();
                Ok(None)
            }
            Err(err) => {
                self.inc(|s| &s.executor_errors);
                self.remove_lock_without_live_entry(username);
                Err(err)
            }
        }
    }

    /// Invalidate cache entry for a username.
    /// Called on auth failure to trigger re-fetch on next attempt.
    pub fn invalidate(&self, username: &str) {
        if self.entries.remove(username).is_some() {
            info!(
                "[{username}@{}] auth_query cache: invalidated",
                self.pool_name
            );
        }
    }

    /// Attempt re-fetch after auth failure (password may have changed).
    /// Returns `Ok(Some(entry))` if re-fetched, `Ok(None)` if rate-limited or user gone.
    ///
    /// Rate limiting: won't re-fetch if last re-fetch was < `min_interval` ago.
    ///
    /// Uses the same per-username lock as `get_or_fetch()` to prevent concurrent
    /// refetches for the same user.
    pub async fn refetch_on_failure(
        &self,
        username: &str,
    ) -> Result<Option<Arc<CacheEntry>>, Error> {
        // Acquire per-username lock (same lock as get_or_fetch)
        let lock = self
            .locks
            .entry(username.to_string())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone();

        let _lock_cleanup = AuthQueryLockCleanupGuard::new(self, username);
        let _guard = lock.lock().await;

        // Check rate limit (under lock to avoid TOCTOU)
        if let Some(entry) = self.entries.get(username) {
            if let Some(last) = entry.last_refetch_at {
                if last.elapsed() < self.min_interval.as_std() {
                    self.inc(|s| &s.cache_rate_limited);
                    warn!(
                        "[{username}@{}] auth_query cache: refetch rate-limited ({} since last)",
                        self.pool_name,
                        format_elapsed(last.elapsed())
                    );
                    return Ok(None); // Rate limited
                }
            }
        }

        // Fetch fresh from PG.
        self.inc(|s| &s.executor_queries);
        self.inc(|s| &s.cache_refetches);
        match self.executor.fetch_credentials(username).await {
            Ok(Some((password_hash, startup_params))) => {
                validate_auth_query_verifier_len(&password_hash, username, &self.pool_name)?;
                let mut entry = CacheEntry::positive(password_hash);
                entry.set_startup_overlay(startup_params);
                entry.last_refetch_at = Some(Instant::now());
                self.dedicated_mode_filter(&mut entry, username);
                // wrap once, store and hand back the same Arc.
                let entry = Arc::new(entry);
                // Insert before drop - see comment in get_or_fetch.
                self.entries
                    .insert(username.to_string(), Arc::clone(&entry));
                self.enforce_size_limit();
                self.drop_dynamic_pool_if_overlay_drifted(username, entry.startup_overlay.map());
                Ok(Some(entry))
            }
            Ok(None) => {
                let mut entry = CacheEntry::negative();
                entry.last_refetch_at = Some(Instant::now());
                self.entries.insert(username.to_string(), Arc::new(entry));
                self.enforce_size_limit();
                Ok(None)
            }
            Err(err) => {
                self.inc(|s| &s.executor_errors);
                self.remove_lock_without_live_entry(username);
                Err(err)
            }
        }
    }

    /// Clear all entries (called on RELOAD when auth_query config changes).
    /// Also resets dedicated-mode warning suppression after reload.
    pub fn clear(&self) {
        self.entries.clear();
        self.locks.clear();
        self.dedicated_warnings.clear();
    }

    /// Store ClientKey for a cached user (called after successful SCRAM auth).
    pub fn set_client_key(&self, username: &str, client_key: Vec<u8>) {
        if let Some(mut entry) = self.entries.get_mut(username) {
            // `Arc::make_mut` mutates in place when this is the
            // sole owner (the common case under the shard write lock) and
            // copy-on-writes only if a concurrent reader still holds a
            // clone of the previous Arc - preserving the previous
            // in-place mutation semantics without leaking the change to
            // that reader's snapshot.
            Arc::make_mut(&mut entry).client_key = Some(client_key);
        }
    }

    /// Get stored ClientKey for a cached user (for SCRAM passthrough).
    pub fn get_client_key(&self, username: &str) -> Option<Vec<u8>> {
        self.entries
            .get(username)
            .and_then(|e| e.client_key.clone())
    }

    /// Synchronous lookup of the cached per-user startup_parameters map.
    /// Returns `None` when there is no positive, unexpired cache entry. This
    /// never queries PostgreSQL or initializes the executor.
    ///
    /// The TTL check prevents replenishment and anticipation from using stale
    /// per-user GUCs after the auth_query row should have expired.
    pub fn peek_startup_parameters<R>(
        &self,
        username: &str,
        f: impl FnOnce(&std::collections::HashMap<String, String>) -> R,
    ) -> Option<R> {
        // Closure-based to avoid cloning the cached HashMap on every
        // backend spawn. The DashMap shard read-lock is held only for the
        // duration of `f`; consumers merge the overlay directly into their
        // owned destination map instead of through an intermediate clone.
        let entry = self.entries.get(username)?;
        if entry.is_negative {
            return None;
        }
        if entry.is_expired(&self.cache_ttl, &self.cache_failure_ttl) {
            return None;
        }
        Some(f(entry.startup_overlay.map()))
    }

    /// Number of cached entries (for metrics/admin).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock fetcher for unit tests.
    /// Pre-configure responses; fetch calls are counted.
    struct MockFetcher {
        responses: DashMap<String, Option<String>>,
        /// Optional per-user startup_parameters map. Surfaced via
        /// `fetch_credentials` so cache-side wiring can be exercised
        /// without standing up a real PG.
        params: DashMap<String, std::collections::HashMap<String, String>>,
        fetch_count: AtomicUsize,
        /// Optional delay to simulate slow PG queries (for concurrency tests).
        delay: std::time::Duration,
    }

    impl MockFetcher {
        fn new() -> Self {
            Self {
                responses: DashMap::new(),
                params: DashMap::new(),
                fetch_count: AtomicUsize::new(0),
                delay: std::time::Duration::ZERO,
            }
        }

        fn with_delay(delay: std::time::Duration) -> Self {
            Self {
                responses: DashMap::new(),
                params: DashMap::new(),
                fetch_count: AtomicUsize::new(0),
                delay,
            }
        }

        fn add_user(&self, username: &str, password_hash: &str) {
            self.responses
                .insert(username.to_string(), Some(password_hash.to_string()));
        }

        fn add_user_with_params(
            &self,
            username: &str,
            password_hash: &str,
            params: &[(&str, &str)],
        ) {
            self.responses
                .insert(username.to_string(), Some(password_hash.to_string()));
            let map: std::collections::HashMap<String, String> = params
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            self.params.insert(username.to_string(), map);
        }

        fn fetch_count(&self) -> usize {
            self.fetch_count.load(Ordering::SeqCst)
        }
    }

    impl PasswordFetcher for MockFetcher {
        fn fetch<'a>(
            &'a self,
            username: &'a str,
        ) -> impl Future<Output = Result<Option<String>, Error>> + Send + 'a {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            let result = self
                .responses
                .get(username)
                .map(|r| r.clone())
                .unwrap_or(None);
            let delay = self.delay;
            async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                Ok(result)
            }
        }

        fn fetch_credentials<'a>(
            &'a self,
            username: &'a str,
        ) -> impl Future<Output = Result<Option<Credentials>, Error>> + Send + 'a {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            let pw = self
                .responses
                .get(username)
                .map(|r| r.clone())
                .unwrap_or(None);
            let params = self
                .params
                .get(username)
                .map(|r| r.clone())
                .unwrap_or_default();
            let delay = self.delay;
            async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                Ok(pw.map(|p| (p, params)))
            }
        }
    }

    struct ErrorFetcher {
        fetch_count: AtomicUsize,
    }

    impl ErrorFetcher {
        fn new() -> Self {
            Self {
                fetch_count: AtomicUsize::new(0),
            }
        }
    }

    impl PasswordFetcher for ErrorFetcher {
        fn fetch<'a>(
            &'a self,
            _username: &'a str,
        ) -> impl Future<Output = Result<Option<String>, Error>> + Send + 'a {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::AuthQueryConnectionError("executor down".into())) }
        }
    }

    fn test_config() -> AuthQueryConfig {
        AuthQueryConfig {
            query: String::new(),
            user: String::new(),
            password: String::new(),
            database: None,
            workers: 1,
            server_user: None,
            server_password: None,
            pool_size: 40,
            min_pool_size: 0,
            cache_ttl: Duration::from_hours(1),
            cache_failure_ttl: Duration::from_secs(30),
            min_interval: Duration::from_secs(1),
        }
    }

    fn make_cache(
        fetcher: Arc<MockFetcher>,
        config: &AuthQueryConfig,
    ) -> AuthQueryCache<MockFetcher> {
        AuthQueryCache::new("test_pool".to_string(), fetcher, config, None)
    }

    #[test]
    fn return_guard_cancel_path_discards_busy_executor_client() {
        let src = include_str!("auth_query.rs");
        let start = src
            .find("impl Drop for ReturnGuard")
            .expect("ReturnGuard Drop impl must exist");
        let end = src[start..]
            .find("/// Executor for running auth_query SELECT statements")
            .map(|offset| start + offset)
            .expect("ReturnGuard Drop impl should precede executor definition");
        let drop_impl = &src[start..end];

        assert!(
            !drop_impl.contains("tx.send(client).await"),
            "cancelled auth_query fetch must not return a possibly busy Client to the executor pool"
        );
        assert!(
            !drop_impl.contains("if !client.is_closed()"),
            "a non-closed Client can still have an in-flight query after task cancellation"
        );
        assert!(
            drop_impl.find("drop(client);")
                < drop_impl.find("AuthQueryExecutor::schedule_reconnect_with("),
            "cancel path must discard the old Client before scheduling reconnect"
        );
    }

    #[test]
    fn fetch_credentials_reconnect_after_guard_disarm_is_detached() {
        let src = include_str!("auth_query.rs");
        let start = src
            .find("pub async fn fetch_credentials")
            .expect("fetch_credentials must exist");
        let end = src[start..]
            .find("/// Backwards-compatible password-only accessor")
            .map(|offset| start + offset)
            .expect("fetch_credentials should precede fetch_password");
        let body = &src[start..end];

        assert!(
            !body.contains("self.try_reconnect().await"),
            "after guard.client.take(), awaiting direct reconnect is cancellation-unsafe"
        );
        assert!(
            body.contains("self.schedule_reconnect("),
            "reconnect after guard disarm must be scheduled into an owned task"
        );
    }

    #[test]
    fn reconnect_task_retries_until_redeposit_or_channel_close() {
        let src = include_str!("auth_query.rs");
        let start = src
            .find("fn schedule_reconnect_with")
            .expect("schedule_reconnect_with must exist");
        let end = src[start..]
            .find("async fn connect")
            .map(|offset| start + offset)
            .expect("schedule_reconnect_with should precede connect");
        let body = &src[start..end];

        assert!(
            body.contains("loop {"),
            "a failed auth_query reconnect must retry instead of permanently shrinking the pool"
        );
        assert!(
            body.contains("tokio::time::sleep"),
            "retrying auth_query reconnects must use backoff, not a tight loop"
        );
        assert!(
            body.contains("tx.is_closed()"),
            "reconnect retry loop must stop when the executor channel closes"
        );
        assert!(
            !body.contains("pool shrinks by 1"),
            "a failed one-shot reconnect must not leave the worker slot permanently drained"
        );
    }

    #[test]
    fn executor_new_checks_worker_cap_before_channel_allocation() {
        let src = include_str!("auth_query.rs");
        let start = src
            .find("pub async fn new")
            .expect("AuthQueryExecutor::new must exist");
        let end = src[start..]
            .find("fn build_pg_config")
            .map(|offset| start + offset)
            .expect("new should precede build_pg_config");
        let body = &src[start..end];
        let guard = body
            .find("MAX_AUTH_QUERY_WORKERS")
            .expect("new must reject oversized workers directly");
        let channel = body
            .find("async_channel::bounded")
            .expect("new must allocate worker channel");

        assert!(
            guard < channel,
            "workers cap must be checked before channel allocation"
        );
    }

    // -- test_cache_hit: second get_or_fetch returns cached, no extra fetch --

    #[tokio::test]
    async fn test_cache_hit() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("alice", "md5abc123");
        let cache = make_cache(fetcher.clone(), &test_config());

        // First call: cache miss → fetches from PG
        let entry = cache.get_or_fetch("alice").await.unwrap().unwrap();
        assert_eq!(entry.password_hash, "md5abc123");
        assert!(!entry.is_negative);
        assert_eq!(fetcher.fetch_count(), 1);

        // Second call: cache hit → no extra fetch
        let entry = cache.get_or_fetch("alice").await.unwrap().unwrap();
        assert_eq!(entry.password_hash, "md5abc123");
        assert_eq!(fetcher.fetch_count(), 1);
    }

    // -- test_cache_miss_fetches: empty cache triggers a fetch --

    #[tokio::test]
    async fn test_cache_miss_fetches() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("bob", "SCRAM-SHA-256$iter:salt$stored:server");
        let cache = make_cache(fetcher.clone(), &test_config());

        assert_eq!(fetcher.fetch_count(), 0);
        let entry = cache.get_or_fetch("bob").await.unwrap().unwrap();
        assert_eq!(entry.password_hash, "SCRAM-SHA-256$iter:salt$stored:server");
        assert_eq!(fetcher.fetch_count(), 1);
    }

    // -- test_cache_ttl_expiration: expired entry triggers re-fetch --

    #[tokio::test]
    async fn test_cache_ttl_expiration() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("alice", "md5abc123");
        let mut config = test_config();
        config.cache_ttl = Duration::from_millis(50);

        let cache = make_cache(fetcher.clone(), &config);

        cache.get_or_fetch("alice").await.unwrap();
        assert_eq!(fetcher.fetch_count(), 1);

        // Wait for TTL to expire
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        cache.get_or_fetch("alice").await.unwrap();
        assert_eq!(fetcher.fetch_count(), 2);
    }

    // -- sweep_expired drops expired entries plus
    //    their locks/dedicated_warnings peers, preventing unbounded
    //    growth on long-lived caches under unique-username pressure
    //    (brute-force probe, multi-tenant SaaS service accounts). --

    #[tokio::test]
    async fn sampled_ttl_sweep_drops_expired_entries() {
        let fetcher = Arc::new(MockFetcher::new());
        for i in 0..20 {
            fetcher.add_user(&format!("user_{i}"), "md5deadbeef");
        }
        let mut config = test_config();
        config.cache_ttl = Duration::from_millis(20);

        let cache = make_cache(fetcher.clone(), &config);

        // Seed all 20 entries.
        for i in 0..20 {
            cache.get_or_fetch(&format!("user_{i}")).await.unwrap();
        }
        assert_eq!(cache.entries.len(), 20);
        assert_eq!(cache.locks.len(), 20);

        // Let them all expire, then call get_or_fetch enough times to
        // trigger several sweeps. Each sweep drains up to SWEEP_BATCH
        // expired entries; 20 / 8 ≈ 3 sweeps; we do
        // SWEEP_INTERVAL * 3 = 384 calls.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        for _ in 0..(super::SWEEP_INTERVAL as usize * 3) {
            // Sweep happens on the sweep_counter % SWEEP_INTERVAL == 0
            // branch in get_or_fetch; trigger by calling with a username
            // that's not in cache (negative-cache path also exercises
            // the sweep gate). Using a fresh name per call also feeds
            // the cache new entries we don't want to keep - but that's
            // fine, they too are subject to sweep when they expire.
            let _ = cache.get_or_fetch("trigger_user").await;
        }

        // The seeded `user_*` entries should have been swept out.
        // Allow some tolerance - sampled sweep walks SWEEP_BATCH*2
        // entries per call so very large caches may need extra cycles.
        let surviving_seeded = (0..20)
            .filter(|i| cache.entries.contains_key(&format!("user_{i}")))
            .count();
        assert!(
            surviving_seeded <= 4,
            "expected sampled sweep to drain most expired entries, \
             {surviving_seeded} of 20 survived"
        );
        // Locks for swept users dropped in lock-step.
        let surviving_locks = (0..20)
            .filter(|i| cache.locks.contains_key(&format!("user_{i}")))
            .count();
        assert_eq!(
            surviving_seeded, surviving_locks,
            "locks must be dropped in lock-step with entries"
        );
    }

    // -- test_negative_cache: user-not-found is cached with cache_failure_ttl --

    #[tokio::test]
    async fn test_negative_cache() {
        let fetcher = Arc::new(MockFetcher::new());
        // "unknown" not added → fetch returns None
        let mut config = test_config();
        config.cache_failure_ttl = Duration::from_millis(50);

        let cache = make_cache(fetcher.clone(), &config);

        // First call: fetch returns None, cached as negative
        assert!(cache.get_or_fetch("unknown").await.unwrap().is_none());
        assert_eq!(fetcher.fetch_count(), 1);

        // Second call: negative cache hit
        assert!(cache.get_or_fetch("unknown").await.unwrap().is_none());
        assert_eq!(fetcher.fetch_count(), 1);

        // Wait for failure TTL to expire
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Should re-fetch
        assert!(cache.get_or_fetch("unknown").await.unwrap().is_none());
        assert_eq!(fetcher.fetch_count(), 2);
    }

    #[tokio::test]
    async fn cache_size_is_hard_capped_under_unique_negative_users() {
        let fetcher = Arc::new(MockFetcher::new());
        let mut config = test_config();
        config.cache_failure_ttl = Duration::from_hours(1);
        let cache = make_cache(fetcher.clone(), &config);

        for i in 0..(super::MAX_AUTH_QUERY_CACHE_ENTRIES + super::SWEEP_INTERVAL as usize) {
            let username = format!("u{i}");
            assert!(cache.get_or_fetch(&username).await.unwrap().is_none());
        }

        assert!(
            cache.entries.len() <= super::MAX_AUTH_QUERY_CACHE_ENTRIES,
            "credential cache must stay bounded under unique misses, len={}",
            cache.entries.len()
        );
        assert!(
            cache.locks.len() <= super::MAX_AUTH_QUERY_CACHE_ENTRIES,
            "per-username lock map must be pruned with cache entries, len={}",
            cache.locks.len()
        );
    }

    #[tokio::test]
    async fn executor_errors_do_not_grow_per_username_lock_map() {
        let fetcher = Arc::new(ErrorFetcher::new());
        let cache = AuthQueryCache::new("test_pool".to_string(), fetcher, &test_config(), None);

        for i in 0..(super::MAX_AUTH_QUERY_CACHE_ENTRIES + super::SWEEP_INTERVAL as usize) {
            let username = format!("err_user_{i}");
            assert!(cache.get_or_fetch(&username).await.is_err());
        }

        assert_eq!(
            cache.entries.len(),
            0,
            "executor errors must not cache entries"
        );
        assert_eq!(
            cache.locks.len(),
            0,
            "executor errors without live cache entries must not retain per-user locks"
        );
    }

    #[tokio::test]
    async fn canceled_get_or_fetch_removes_lock_without_cache_entry() {
        let fetcher = Arc::new(MockFetcher::with_delay(std::time::Duration::from_secs(30)));
        fetcher.add_user("slow_user", "md5abc123");
        let cache = Arc::new(make_cache(fetcher.clone(), &test_config()));

        let task = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.get_or_fetch("slow_user").await })
        };

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if cache.locks.contains_key("slow_user") && fetcher.fetch_count() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("slow fetch must install a per-user lock before test cancellation");

        task.abort();
        let _ = task.await;

        assert_eq!(
            cache.entries.len(),
            0,
            "aborted fetch must not cache entries"
        );
        assert!(
            !cache.locks.contains_key("slow_user"),
            "aborted fetch without a live cache entry must not retain its per-user lock"
        );
    }

    // -- test_invalidate: removes entry, next fetch goes to PG --

    #[tokio::test]
    async fn test_invalidate() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("alice", "md5abc123");
        let cache = make_cache(fetcher.clone(), &test_config());

        cache.get_or_fetch("alice").await.unwrap();
        assert_eq!(fetcher.fetch_count(), 1);

        cache.invalidate("alice");

        cache.get_or_fetch("alice").await.unwrap();
        assert_eq!(fetcher.fetch_count(), 2);
    }

    // -- test_rate_limiting: refetch_on_failure respects min_interval --

    #[tokio::test]
    async fn test_rate_limiting() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("alice", "md5abc123");
        let mut config = test_config();
        config.min_interval = Duration::from_secs(10);

        let cache = make_cache(fetcher.clone(), &config);

        // First refetch: no previous refetch → succeeds
        let result = cache.refetch_on_failure("alice").await.unwrap();
        assert!(result.is_some());
        assert_eq!(fetcher.fetch_count(), 1);

        // Second refetch immediately: rate-limited
        let result = cache.refetch_on_failure("alice").await.unwrap();
        assert!(result.is_none());
        assert_eq!(fetcher.fetch_count(), 1); // No additional fetch
    }

    // -- test_double_checked_locking: concurrent requests → single fetch --

    #[tokio::test]
    async fn test_double_checked_locking() {
        let fetcher = Arc::new(MockFetcher::with_delay(std::time::Duration::from_millis(
            100,
        )));
        fetcher.add_user("alice", "md5abc123");
        let cache = Arc::new(make_cache(fetcher.clone(), &test_config()));

        // Spawn concurrent requests for the same user
        let mut handles = Vec::new();
        for _ in 0..10 {
            let cache = cache.clone();
            handles.push(tokio::spawn(
                async move { cache.get_or_fetch("alice").await },
            ));
        }

        for handle in handles {
            let result = handle.await.unwrap().unwrap().unwrap();
            assert_eq!(result.password_hash, "md5abc123");
        }

        // Double-checked locking: only one fetch despite 10 concurrent requests
        assert_eq!(fetcher.fetch_count(), 1);
    }

    // -- test_long_username_rejected: >63 chars → None without fetch or caching --

    #[tokio::test]
    async fn test_long_username_rejected() {
        let fetcher = Arc::new(MockFetcher::new());
        let cache = make_cache(fetcher.clone(), &test_config());

        let long_name = "a".repeat(MAX_USERNAME_LEN + 1);
        let result = cache.get_or_fetch(&long_name).await.unwrap();
        assert!(result.is_none());
        assert_eq!(fetcher.fetch_count(), 0); // No fetch attempted
        assert_eq!(cache.len(), 0); // Not cached
    }

    #[tokio::test]
    async fn oversized_password_hash_rejected_without_cache_entry() {
        let fetcher = Arc::new(MockFetcher::new());
        let oversized_hash = "x".repeat(4097);
        fetcher.add_user("alice", &oversized_hash);
        let cache = make_cache(fetcher.clone(), &test_config());

        let err = cache.get_or_fetch("alice").await.unwrap_err();
        match err {
            Error::AuthQueryConfigError(msg) => {
                assert!(msg.contains("password verifier"));
                assert!(msg.contains("exceeds"));
            }
            other => panic!("expected AuthQueryConfigError, got {other:?}"),
        }
        assert_eq!(fetcher.fetch_count(), 1);
        assert_eq!(cache.len(), 0);
    }

    // -- test_clear: removes all entries and locks --

    #[tokio::test]
    async fn test_clear() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("alice", "md5abc123");
        fetcher.add_user("bob", "md5def456");
        let cache = make_cache(fetcher.clone(), &test_config());

        cache.get_or_fetch("alice").await.unwrap();
        cache.get_or_fetch("bob").await.unwrap();
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    // -- test_set_client_key: stores SCRAM ClientKey on existing entry --

    #[tokio::test]
    async fn test_set_client_key() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("alice", "SCRAM-SHA-256$iter:salt$stored:server");
        let cache = make_cache(fetcher.clone(), &test_config());

        let entry = cache.get_or_fetch("alice").await.unwrap().unwrap();
        assert!(entry.client_key.is_none());

        let key = vec![1, 2, 3, 4];
        cache.set_client_key("alice", key.clone());

        let entry = cache.get_or_fetch("alice").await.unwrap().unwrap();
        assert_eq!(entry.client_key, Some(key));
    }

    // -- test_stats_counters: verifies stats are incremented correctly --

    #[tokio::test]
    async fn test_stats_counters() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user("alice", "md5abc123");
        let stats = Arc::new(AuthQueryStats::default());
        let cache = AuthQueryCache::new(
            "test_pool".to_string(),
            fetcher.clone(),
            &test_config(),
            Some(stats.clone()),
        );

        // Cache miss → executor_queries + cache_misses
        cache.get_or_fetch("alice").await.unwrap();
        assert_eq!(stats.cache_misses.load(Ordering::Relaxed), 1);
        assert_eq!(stats.executor_queries.load(Ordering::Relaxed), 1);
        assert_eq!(stats.cache_hits.load(Ordering::Relaxed), 0);

        // Cache hit
        cache.get_or_fetch("alice").await.unwrap();
        assert_eq!(stats.cache_hits.load(Ordering::Relaxed), 1);
        assert_eq!(stats.executor_queries.load(Ordering::Relaxed), 1); // no new query

        // Refetch
        cache.refetch_on_failure("alice").await.unwrap();
        assert_eq!(stats.cache_refetches.load(Ordering::Relaxed), 1);
        assert_eq!(stats.executor_queries.load(Ordering::Relaxed), 2);

        // Rate-limited refetch (min_interval = 1s, immediately after first refetch)
        cache.refetch_on_failure("alice").await.unwrap();
        assert_eq!(stats.cache_rate_limited.load(Ordering::Relaxed), 1);
        assert_eq!(stats.executor_queries.load(Ordering::Relaxed), 2); // no new query
    }

    // -- parse_startup_parameters_text: pure-parser unit tests --

    #[test]
    fn parse_startup_parameters_absent_column_returns_empty() {
        let r = AuthQueryExecutor::parse_startup_parameters_text(None, "u", "p");
        assert!(r.is_empty());
    }

    #[test]
    fn parse_startup_parameters_empty_string_returns_empty() {
        let r = AuthQueryExecutor::parse_startup_parameters_text(Some(""), "u", "p");
        assert!(r.is_empty());
    }

    #[test]
    fn parse_startup_parameters_simple_json_object() {
        let r = AuthQueryExecutor::parse_startup_parameters_text(
            Some(r#"{"plan_cache_mode":"force_custom_plan","work_mem":"64MB"}"#),
            "u",
            "p",
        );
        assert_eq!(
            r.get("plan_cache_mode").map(String::as_str),
            Some("force_custom_plan")
        );
        assert_eq!(r.get("work_mem").map(String::as_str), Some("64MB"));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn parse_startup_parameters_reserved_key_dropped() {
        // 'user' is reserved by pg_doorman; the valid sibling key survives.
        let r = AuthQueryExecutor::parse_startup_parameters_text(
            Some(r#"{"user":"x","work_mem":"64MB"}"#),
            "u",
            "p",
        );
        assert!(!r.contains_key("user"));
        assert_eq!(r.get("work_mem").map(String::as_str), Some("64MB"));
    }

    #[test]
    fn parse_startup_parameters_non_string_values_dropped() {
        // number, boolean, null, array, object on the right-hand side are
        // all rejected; only string-valued entries survive.
        let r = AuthQueryExecutor::parse_startup_parameters_text(
            Some(
                r#"{"work_mem":64,"on":true,"off":null,"arr":[1],"obj":{},"plan_cache_mode":"force_custom_plan"}"#,
            ),
            "u",
            "p",
        );
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.get("plan_cache_mode").map(String::as_str),
            Some("force_custom_plan")
        );
    }

    #[test]
    fn parse_startup_parameters_malformed_json_returns_empty() {
        let r = AuthQueryExecutor::parse_startup_parameters_text(Some("not-json"), "u", "p");
        assert!(r.is_empty());
    }

    #[test]
    fn parse_startup_parameters_non_object_returns_empty() {
        let r = AuthQueryExecutor::parse_startup_parameters_text(Some("[1,2,3]"), "u", "p");
        assert!(r.is_empty());
    }

    #[test]
    fn parse_startup_parameters_oversize_text_returns_empty() {
        // A pathological auth_query row should not make serde_json walk
        // megabytes of JSON. The raw text cap matches `MAX_OPERATOR_BUDGET`,
        // so anything past that returns empty before parsing starts. Use a
        // giant string so the byte length crosses the cap independently of
        // JSON shape.
        let cap = crate::config::startup_parameters::MAX_OPERATOR_BUDGET;
        let bytes = "a".repeat(cap + 1);
        let r = AuthQueryExecutor::parse_startup_parameters_text(Some(&bytes), "u", "p");
        assert!(
            r.is_empty(),
            "oversize raw column must be rejected before serde_json walks it"
        );
    }

    #[test]
    fn limited_text_rejects_oversize_raw_before_string_decode() {
        let cap = crate::config::startup_parameters::MAX_OPERATOR_BUDGET;
        let raw = vec![b'a'; cap + 1];
        let err = <LimitedText as FromSql>::from_sql(&Type::TEXT, &raw)
            .expect_err("oversize raw text must fail before String allocation")
            .to_string();
        assert!(err.contains(LIMITED_TEXT_OVERSIZE_TAG));
        assert!(err.contains("exceeding operator budget"));
    }

    #[test]
    fn extract_startup_parameters_text_branch_uses_limited_decoder() {
        let src = include_str!("auth_query.rs");
        let start = src
            .find("fn extract_startup_parameters(")
            .expect("extract_startup_parameters must exist");
        let body = &src[start..];
        let end = body
            .find("\n    /// Parse the optional `startup_parameters`")
            .expect("parser docs should follow extractor");
        let body = &body[..end];

        assert!(
            body.contains("Option<LimitedText>"),
            "text startup_parameters must use the bounded raw decoder"
        );
        assert!(
            !body.contains("Option<String>"),
            "text startup_parameters must not allocate String before the raw budget check"
        );
    }

    #[test]
    fn parse_startup_parameters_invalid_guc_name_dropped() {
        // Keys with spaces fail the shared `is_valid_guc_name` check used
        // for operator-supplied parameter maps.
        let r = AuthQueryExecutor::parse_startup_parameters_text(
            Some(r#"{"bad name":"x","plan_cache_mode":"force_custom_plan"}"#),
            "u",
            "p",
        );
        assert!(!r.contains_key("bad name"));
        assert!(r.contains_key("plan_cache_mode"));
    }

    #[test]
    fn parse_startup_parameters_null_byte_value_dropped() {
        // A null byte in the value fails the shared validator; the good
        // neighbor still survives.
        let r = AuthQueryExecutor::parse_startup_parameters_text(
            Some("{\"work_mem\":\"64\\u0000MB\",\"plan_cache_mode\":\"force_custom_plan\"}"),
            "u",
            "p",
        );
        assert!(!r.contains_key("work_mem"));
        assert!(r.contains_key("plan_cache_mode"));
    }

    // -- dedicated_mode_filter: drops params + warns once per username --

    #[tokio::test]
    async fn dedicated_mode_filter_drops_params_and_warns_once() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user_with_params("alice", "md5abc123", &[("work_mem", "64MB")]);
        let mut config = test_config();
        // Mark the config as dedicated by providing a server_user.
        config.server_user = Some("doorman_backend".to_string());

        let cache = make_cache(fetcher.clone(), &config);

        // Cache miss path applies the filter: per-user params are dropped
        // because the backend identity is shared in dedicated mode.
        let entry = cache.get_or_fetch("alice").await.unwrap().unwrap();
        assert!(
            entry.startup_overlay.is_empty(),
            "params must be cleared in dedicated mode"
        );

        // The warning fires at most once per username: subsequent calls do
        // not insert into dedicated_warnings again. We assert that the
        // tracker still holds exactly one entry after a second miss-and-fill.
        cache.invalidate("alice");
        let entry = cache.get_or_fetch("alice").await.unwrap().unwrap();
        assert!(entry.startup_overlay.is_empty());
        assert_eq!(cache.dedicated_warnings.len(), 1);

        // clear() resets the warning tracker so a config reload re-arms it.
        cache.clear();
        assert_eq!(cache.dedicated_warnings.len(), 0);
    }

    #[tokio::test]
    async fn non_dedicated_mode_keeps_params() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user_with_params("alice", "md5abc123", &[("work_mem", "64MB")]);
        let config = test_config(); // server_user = None: passthrough mode

        let cache = make_cache(fetcher.clone(), &config);
        let entry = cache.get_or_fetch("alice").await.unwrap().unwrap();
        assert_eq!(
            entry
                .startup_overlay
                .map()
                .get("work_mem")
                .map(String::as_str),
            Some("64MB")
        );
    }

    // ---------------------------------------------------------------------
    // peek_startup_parameters: sync, non-fetching lookup used by backend spawn
    // ---------------------------------------------------------------------

    // Closure-based API tested by snapshotting the borrowed HashMap into
    // an owned one when an existing assertion needs to inspect contents.
    // Generic over the cache's fetcher because the test harness uses a
    // `MockFetcher` rather than the production `AuthQueryExecutor`.
    fn peek_snapshot<F>(
        cache: &AuthQueryCache<F>,
        username: &str,
    ) -> Option<std::collections::HashMap<String, String>>
    where
        F: PasswordFetcher,
    {
        cache.peek_startup_parameters(username, |m| m.clone())
    }

    #[tokio::test]
    async fn peek_startup_parameters_missing_user_returns_none() {
        let fetcher = Arc::new(MockFetcher::new());
        let config = test_config();
        let cache = make_cache(fetcher, &config);
        assert!(peek_snapshot(&cache, "alice").is_none());
    }

    #[tokio::test]
    async fn peek_startup_parameters_negative_entry_returns_none() {
        let fetcher = Arc::new(MockFetcher::new());
        // No user added; first lookup populates a negative cache entry.
        let config = test_config();
        let cache = make_cache(fetcher, &config);
        assert!(cache.get_or_fetch("ghost").await.unwrap().is_none());
        assert!(peek_snapshot(&cache, "ghost").is_none());
    }

    #[tokio::test]
    async fn peek_startup_parameters_returns_none_for_expired_entry() {
        // A positive cache entry that has lived past `cache_ttl` must not
        // pin a stale per-user startup parameter onto a backend spawned by
        // the replenishment loop. Mirrors `test_cache_ttl_expiration` but
        // exercises the peek path used by backend startup.
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user_with_params("alice", "md5abc123", &[("work_mem", "64MB")]);
        let mut config = test_config();
        config.cache_ttl = Duration::from_millis(50);

        let cache = make_cache(fetcher, &config);
        cache.get_or_fetch("alice").await.unwrap().unwrap();
        // Verify that peek sees the entry before it expires.
        assert!(peek_snapshot(&cache, "alice").is_some());

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        assert!(
            peek_snapshot(&cache, "alice").is_none(),
            "peek must return None once cache_ttl has elapsed for the entry"
        );
    }

    #[tokio::test]
    async fn peek_startup_parameters_positive_entry_returns_map() {
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user_with_params(
            "alice",
            "md5abc123",
            &[("work_mem", "64MB"), ("statement_timeout", "10s")],
        );
        let config = test_config();
        let cache = make_cache(fetcher, &config);
        cache.get_or_fetch("alice").await.unwrap().unwrap();

        let params = peek_snapshot(&cache, "alice").unwrap();
        assert_eq!(params.get("work_mem").map(String::as_str), Some("64MB"));
        assert_eq!(
            params.get("statement_timeout").map(String::as_str),
            Some("10s")
        );
    }

    #[tokio::test]
    async fn peek_startup_parameters_dedicated_mode_returns_empty() {
        // Dedicated mode keeps the user cached but removes per-user params.
        let fetcher = Arc::new(MockFetcher::new());
        fetcher.add_user_with_params("alice", "md5abc123", &[("work_mem", "64MB")]);
        let mut config = test_config();
        config.server_user = Some("shared".to_string());
        config.server_password = Some("secret".to_string());

        let cache = make_cache(fetcher, &config);
        cache.get_or_fetch("alice").await.unwrap().unwrap();

        let params = peek_snapshot(&cache, "alice").unwrap();
        assert!(params.is_empty());
    }
}
