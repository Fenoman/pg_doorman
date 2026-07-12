use bytes::{BufMut, BytesMut};
use log::{debug, log_enabled, trace, warn, Level};
use std::convert::TryInto;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::config::config_arc;
use crate::errors::Error;
use crate::messages::{error_response_timeout, Bind, Close, Describe, Parse};
use crate::pool::ConnectionPool;
use crate::server::{now_monotonic_ms, Server};
use crate::utils::strings::truncate_query_for_log;

use super::util::{
    contains_discard_all, extract_reset_cleanup_commands, extract_set_cleanup_commands,
};

/// Replacement query for extended-protocol `DISCARD ALL` interception
/// Any zero-parameter, side-effect-free SQL that the
/// backend will accept via `Parse` works; `SELECT 1` is the minimal
/// such statement. The substitution preserves the backend's prepared-
/// statement cache and planner state while letting the client's
/// `ParseComplete -> BindComplete -> DataRow -> CommandComplete ->
/// ReadyForQuery` flow come from the real backend.
///
/// Trade-off note: the `CommandComplete` tag the client receives will
/// be `SELECT 1`, not `DISCARD ALL`. Clients that strictly check the
/// tag may notice - for them the per-pool `intercept_discard_all =
/// false` switch is the escape hatch. In return we avoid the protocol-
/// synthesis complexity of fabricating `ParseComplete` /
/// `BindComplete` / `CommandComplete("DISCARD ALL")` from pg_doorman
/// and inherit the backend's response
/// ordering / batch handling for free.
const EXT_DISCARD_ALL_NOOP: &str = "SELECT 1";

/// Throttle for the synthetic-miss WARN line. Without rate-limiting a
/// driver hammering the pooler at 10k RPS would write 10k WARN lines per
/// second, drowning the log pipeline. One line per 10 seconds is enough
/// for an operator to notice; the rest fall to DEBUG so they can still
/// be reconstructed under elevated log level. Counter
/// `pg_doorman_query_interner_synthetic_misses_total` increments on
/// every miss regardless.
const SYNTHETIC_MISS_WARN_INTERVAL_MS: u64 = 10_000;
static SYNTHETIC_MISS_LAST_WARN_MS: AtomicU64 = AtomicU64::new(0);

fn synthetic_miss_should_warn() -> bool {
    let now = now_monotonic_ms();
    let last = SYNTHETIC_MISS_LAST_WARN_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= SYNTHETIC_MISS_WARN_INTERVAL_MS {
        SYNTHETIC_MISS_LAST_WARN_MS.store(now, Ordering::Relaxed);
        true
    } else {
        false
    }
}

use super::core::{
    BatchOperation, CachedStatement, Client, ParseCompleteTarget, PreparedStatementKey, PutOutcome,
    SkippedParse,
};
use super::PREPARED_STATEMENT_COUNTER;

fn replacement_close_target(
    previous: &CachedStatement,
    replacement_server_name: &str,
) -> Option<String> {
    let previous_server_name = previous.server_name();
    if previous_server_name == replacement_server_name {
        None
    } else {
        Some(previous_server_name.to_string())
    }
}

fn first_set_cleanup_command(query: &[u8]) -> Option<crate::server::cleanup::SetCleanupCommand> {
    extract_set_cleanup_commands(query).first().copied()
}

fn first_reset_cleanup_command(
    query: &[u8],
) -> Option<crate::server::cleanup::ResetCleanupCommand> {
    extract_reset_cleanup_commands(query).first().copied()
}

fn disabled_parse_name_and_query(message: &BytesMut) -> Option<(String, &[u8], usize)> {
    if message.len() < 9 || message.first().copied() != Some(b'P') {
        return None;
    }
    let declared_len =
        u32::from_be_bytes([message[1], message[2], message[3], message[4]]) as usize;
    if declared_len != message.len().saturating_sub(1) {
        return None;
    }

    let name_start = 5;
    let name_nul = message[name_start..].iter().position(|&byte| byte == 0)? + name_start;
    let name_len = name_nul - name_start;
    if name_len > crate::messages::extended::MAX_PARSE_NAME_BYTES {
        return None;
    }
    let statement_name = std::str::from_utf8(&message[name_start..name_nul])
        .ok()?
        .to_string();

    let query_start = name_nul + 1;
    let query_nul = message[query_start..].iter().position(|&byte| byte == 0)? + query_start;
    let params_start = query_nul + 1;
    let num_params_bytes = message.get(params_start..params_start + 2)?;
    let num_params = i16::from_be_bytes([num_params_bytes[0], num_params_bytes[1]]);
    if num_params < 0 {
        return None;
    }
    let params_len = (num_params as usize).checked_mul(4)?;
    if params_start + 2 + params_len != message.len() {
        return None;
    }

    Some((
        statement_name,
        &message[query_start..query_nul],
        num_params as usize,
    ))
}

fn execute_portal_name(message: &BytesMut) -> Option<&str> {
    if message.len() < 10 || message.first().copied() != Some(b'E') {
        return None;
    }
    let body = &message[5..];
    let nul_pos = body.iter().position(|b| *b == 0)?;
    if body.len().saturating_sub(nul_pos + 1) != 4 {
        return None;
    }
    std::str::from_utf8(&body[..nul_pos]).ok()
}

impl<S, T> Client<S, T>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    /// Makes sure the checked out server has the prepared statement and sends it to the server if it doesn't
    ///
    /// Looks the statement up in the client cache and delegates to
    /// [`Self::ensure_prepared_statement_is_on_server_cached`]. Hot-path
    /// callers (`process_bind_immediate` / `process_describe_immediate`)
    /// that have already cloned the [`CachedStatement`] call the `_cached`
    /// variant directly to avoid this second lookup + clone.
    /// Retained as the lookup-and-delegate entry point / test seam.
    #[allow(dead_code)]
    pub(crate) async fn ensure_prepared_statement_is_on_server(
        &mut self,
        key: PreparedStatementKey,
        pool: &ConnectionPool,
        server: &mut Server,
    ) -> Result<(), Error> {
        match self.prepared.cache.get(&key).cloned() {
            Some(cached) => {
                self.ensure_prepared_statement_is_on_server_cached(&cached, key, pool, server)
                    .await
            }
            None => Err(Error::ClientError(format!(
                "prepared statement `{key:?}` not found"
            ))),
        }
    }

    /// Same as [`Self::ensure_prepared_statement_is_on_server`] but the
    /// caller supplies the already-looked-up [`CachedStatement`], skipping
    /// the second ahash lookup + clone on the Bind/Describe hot path
    ///. `key` is still required: on `PreparedStatementError`
    /// the rejected entry must be popped from the client cache (and the
    /// stats refreshed) to prevent a later SQLSTATE 26000 desync.
    pub(crate) async fn ensure_prepared_statement_is_on_server_cached(
        &mut self,
        cached: &CachedStatement,
        key: PreparedStatementKey,
        pool: &ConnectionPool,
        server: &mut Server,
    ) -> Result<(), Error> {
        if log_enabled!(Level::Debug) {
            debug!(
                "[{}@{} #c{}] client cache hit: {} query=\"{}\"",
                self.username,
                self.pool_name,
                self.connection_id,
                match &key {
                    PreparedStatementKey::Named(name) => format!("name=`{name}`"),
                    PreparedStatementKey::Anonymous(hash) => format!("hash={hash:#x} (unnamed)"),
                },
                truncate_query_for_log(cached.parse.query()),
            );
        }
        // Get the server-side name (may be async_name for async clients).
        // Borrow it as &str -- register_parse_to_server_cache takes
        // &str, so the per-Bind/Describe String allocation is unnecessary.
        // server_name borrows `cached` (a separate borrow from the
        // `server: &mut Server` argument), so there is no borrow conflict.
        let server_name = cached.server_name();
        // In this case we want to send the parse message to the server
        // since pgcat is initiating the prepared statement on this specific server
        match self
            .register_parse_to_server_cache(
                true,
                &cached.hash,
                &cached.parse,
                server_name,
                pool,
                server,
            )
            .await
        {
            Ok(_) => (),
            Err(err) => match err {
                Error::PreparedStatementError => {
                    warn!("[{}@{} #c{}] server rejected prepared statement {:?}, evicting from client cache", self.username, self.pool_name, self.connection_id, key);
                    self.prepared.cache.pop(&key);
                    // Cache shrank - refresh ClientStats snapshot so
                    // SHOW POOLS / Prometheus don't keep showing the
                    // pre-eviction count until the next Parse.
                    self.update_prepared_cache_stats();
                    // earlier this branch fell
                    // through with `Ok(())`. Callers
                    // (process_bind_immediate /
                    // process_describe_immediate) then
                    // enqueued the Bind/Describe into
                    // `self.buffer` against the
                    // server-side name the backend just
                    // rejected - the next Sync produced
                    // SQLSTATE 26000 AFTER earlier
                    // ParseComplete responses had been
                    // forwarded, desyncing the driver.
                    // Propagate so the outer match
                    // mark_bad's the server and the client
                    // gets a clean error instead of stream
                    // corruption.
                    return Err(Error::PreparedStatementError);
                }

                _ => {
                    return Err(err);
                }
            },
        }

        Ok(())
    }

    /// Register the parse to the server cache and send it to the server if requested (ie. requested by pgcat)
    ///
    /// Also updates the pool LRU that this parse was used recently
    ///
    /// # Arguments
    /// * `should_send_parse_to_server` - Whether to actually send Parse to server
    /// * `hash` - Hash of the statement for pool LRU promotion
    /// * `parse` - The Parse message containing query text and parameters
    /// * `server_name` - The name to use on the server (may differ from parse.name for async clients)
    /// * `pool` - Connection pool for LRU promotion
    /// * `server` - Server connection to register on
    pub(crate) async fn register_parse_to_server_cache(
        &self,
        should_send_parse_to_server: bool,
        hash: &u64,
        parse: &Arc<Parse>,
        server_name: &str,
        pool: &ConnectionPool,
        server: &mut Server,
    ) -> Result<(), Error> {
        // We want to promote this in the pool's LRU
        pool.promote_prepared_statement_hash(hash);

        debug!(
            "[{}@{} #c{}] checking server connection cache for statement `{}`",
            self.username, self.pool_name, self.connection_id, server_name
        );

        server
            .register_prepared_statement(parse, server_name, should_send_parse_to_server)
            .await?;

        Ok(())
    }

    /// Process Parse message immediately without buffering.
    /// Adds data directly to self.buffer or response_message_queue_buffer for cached statements.
    pub(crate) async fn process_parse_immediate(
        &mut self,
        message: BytesMut,
        pool: &ConnectionPool,
        server: &mut Server,
    ) -> Result<(), Error> {
        // cap pending pipelined extended-protocol buffer.
        crate::client::transaction::enforce_extended_batch_buffer_cap(
            self.buffer.len(),
            message.len(),
            "Parse",
        )?;
        // In disabled-cache mode, inspect the raw Parse fields without
        // allocating the query text. Only DISCARD ALL takes the full Parse
        // decode and reserialization path.
        if !self.prepared.enabled {
            debug!(
                "[{}@{} #c{}] prepared statements disabled, forwarding Parse",
                self.username, self.pool_name, self.connection_id,
            );
            let first_char_in_name = *message.get(5).unwrap_or(&0);
            let (intercepted_discard_all, cleanup_attribution) =
                match disabled_parse_name_and_query(&message) {
                    Some((statement_name, query, num_params)) => {
                        let intercepted = self.transaction_mode
                            && server.intercept_discard_all()
                            && !server.in_transaction()
                            && !server.in_copy_mode()
                            && num_params == 0
                            && contains_discard_all(query);
                        let set_command = first_set_cleanup_command(query);
                        let reset_command = if set_command.is_none() {
                            first_reset_cleanup_command(query)
                        } else {
                            None
                        };
                        (
                            intercepted,
                            Some((statement_name, set_command, reset_command)),
                        )
                    }
                    None => (false, None),
                };

            let message = if intercepted_discard_all {
                let parse: Parse = (&message).try_into()?;
                debug!(
                    "[{}@{} #c{}] extended-protocol DISCARD ALL intercepted (name=`{}`): \
                     rewriting Parse query to {:?}",
                    self.username,
                    self.pool_name,
                    self.connection_id,
                    parse.name,
                    EXT_DISCARD_ALL_NOOP,
                );
                pool.address.stats.discard_all_intercepted();
                let rewritten = parse.with_replaced_query(EXT_DISCARD_ALL_NOOP);
                rewritten.to_bytes_with_name(&rewritten.name)?
            } else {
                message
            };

            if first_char_in_name != 0 {
                // This is a named prepared statement while prepared statements are disabled
                // Server connection state will need to be cleared at checkin
                server.mark_dirty();
            }
            if let Some((statement_name, set_command, reset_command)) = cleanup_attribution {
                if let Some(command) = set_command {
                    self.prepared.track_disabled_statement_set_cleanup_command(
                        statement_name.clone(),
                        command,
                    )?;
                } else if let Some(command) = reset_command {
                    self.prepared
                        .track_disabled_statement_reset_cleanup_command(
                            statement_name.clone(),
                            command,
                        )?;
                } else {
                    self.prepared
                        .remove_disabled_statement_cleanup_command(statement_name.as_str());
                }
            }
            // Add directly to buffer
            self.buffer.put(&message[..]);
            // Track operation for correct expected_responses calculation in Flush
            self.prepared
                .batch_operations
                .push(BatchOperation::ParseSent {
                    statement_name: Arc::<str>::from(""),
                });
            return Ok(());
        }

        // A single parse pass. `Parse::try_from` already scans
        // and allocates `parse.name` from the same first NUL-terminated
        // field that `Parse::get_name` returns (pinned by
        // parse_get_name_equals_parsed_name_*), so cloning `parse.name`
        // avoids a redundant second scan + String allocation per Parse. The
        // clone happens before the DISCARD-ALL block below rebinds `parse`
        // (which preserves `.name`) and would otherwise move it.
        let parse: Parse = (&message).try_into()?;
        let client_given_name = parse.name.clone();

        // Extended-protocol DISCARD ALL interception. Same gate semantics as the simple-query
        // intercept in `transaction.rs::handle_simple_query`:
        //   * transaction pooling mode (session pool must let the backend
        //     clear its real session state),
        //   * not currently inside a transaction (otherwise DISCARD ALL
        //     is itself an error that the backend must surface),
        //   * not mid-COPY,
        //   * per-pool `intercept_discard_all` switch is on (default true),
        //   * the Parse query text parses as `DISCARD ALL` modulo whitespace,
        //     leading/trailing `--`/`/* */` comments, trailing semicolons
        //     (same `contains_discard_all` parser the simple-query path
        //     uses - see `client::util::contains_discard_all`),
        //   * the Parse declares zero parameters (DISCARD ALL always does;
        //     a non-zero count is a malformed client message that we leave
        //     to the backend to reject instead of silently reshaping into
        //     a no-op with mismatched parameter arity).
        //
        // When all gates pass we rewrite the Parse's query text to
        // `EXT_DISCARD_ALL_NOOP` (`SELECT 1`) and let the rest of the
        // pipeline forward it to the backend normally. The backend executes
        // a one-row SELECT instead of `DISCARD ALL` - so its prepared-
        // statement cache, planner state, advisory locks, session-temp
        // tables, and pg_variables state are all preserved (the entire
        // point of the iServ contract). The client still gets a valid
        // `ParseComplete + BindComplete + DataRow + CommandComplete +
        // ReadyForQuery` exchange from the real backend.
        let intercepted_discard_all = self.transaction_mode
            && server.intercept_discard_all()
            && !server.in_transaction()
            && !server.in_copy_mode()
            && parse.num_params() == 0
            && contains_discard_all(parse.query().as_bytes());
        let parse = if intercepted_discard_all {
            debug!(
                "[{}@{} #c{}] extended-protocol DISCARD ALL intercepted (name=`{}`): \
                 rewriting Parse query to {:?} to preserve backend cache state",
                self.username,
                self.pool_name,
                self.connection_id,
                client_given_name,
                EXT_DISCARD_ALL_NOOP,
            );
            pool.address.stats.discard_all_intercepted();
            parse.with_replaced_query(EXT_DISCARD_ALL_NOOP)
        } else {
            parse
        };

        // Include startup-time planner state in the pool cache key.
        // Clients with different search_path/role settings must get
        // different server-side prepared statements.
        let planner_hash = self.server_parameters.planner_param_hash();
        let hash = parse.get_hash_with_planner_params(planner_hash);

        // Always use pool cache to get shared Arc<Parse> (saves memory for async clients too)
        let name_arg = if client_given_name.is_empty() {
            None
        } else {
            Some(client_given_name.as_str())
        };
        let shared_parse = match pool.register_parse_to_cache(hash, &parse, name_arg, planner_hash)
        {
            Some(parse) => parse,
            None => {
                return Err(Error::ClientError(format!(
                    "Could not store Prepared statement `{client_given_name}`"
                )))
            }
        };

        // For async clients, generate a unique name to avoid "prepared statement already exists" errors
        // The query text is still shared via Arc<Parse> from pool cache.
        // build the name as `Arc<str>` directly so every downstream
        // clone (CachedStatement.async_name, BatchOperation::*,
        // SkippedParse.statement_name) is a refcount bump instead of a
        // fresh String allocation per Bind/Describe/Close roundtrip.
        let async_name: Option<Arc<str>> = if self.prepared.async_client {
            Some(Arc::<str>::from(
                format!(
                    // Only uniqueness matters for the generated async name,
                    // not inter-thread ordering, and fetch_add is atomic
                    // under any ordering. Relaxed avoids the full SeqCst
                    // fence on this per-Parse path (matches the sibling
                    // counter use in messages/extended.rs).
                    "DOORMAN_async_{}",
                    PREPARED_STATEMENT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                )
                .as_str(),
            ))
        } else {
            None
        };

        if log_enabled!(Level::Debug) {
            debug!(
                "[{}@{} #c{}] mapped statement `{}` -> `{}` (hash={:#x}{}) query=\"{}\"",
                self.username,
                self.pool_name,
                self.connection_id,
                client_given_name,
                shared_parse.name,
                hash,
                async_name
                    .as_ref()
                    .map(|n| format!(", async_name={n}"))
                    .unwrap_or_default(),
                truncate_query_for_log(shared_parse.query()),
            );
        }

        // For anonymous prepared statements, use hash as key to avoid collisions
        // Save hash for anonymous prepared statement lookup
        if client_given_name.is_empty() {
            self.prepared.last_anonymous_hash = Some(hash);
        }
        let cache_key = PreparedStatementKey::from_name_or_hash(client_given_name, hash);

        // Determine the server-side statement name.
        // `Arc<str>` so every downstream `.clone()` (skipped_parses,
        // batch_operations) is a refcount bump. Async path bumps the
        // refcount of the cached async_name; non-async path still
        // allocates once per Parse from shared_parse.name (a String) -
        // that allocation disappears once Parse.name itself migrates
        // to Arc<str>.
        let server_stmt_name: Arc<str> = match &async_name {
            Some(a) => Arc::clone(a),
            None => Arc::<str>::from(shared_parse.name.as_str()),
        };

        let cached = CachedStatement {
            parse: shared_parse.clone(),
            hash,
            intercepted_discard_all,
            set_cleanup_command: first_set_cleanup_command(parse.query().as_bytes()),
            reset_cleanup_command: first_reset_cleanup_command(parse.query().as_bytes()),
            async_name: async_name.clone(),
        };
        // distinguish three real eviction modes:
        //   * Anonymous LRU eviction (normal capacity pressure, bump
        //     `anonymous_evictions` + observe metric).
        //   * Named cap eviction (hard cap reached) - bump a
        //     SEPARATE `named_evictions` counter, log distinctly, AND
        //     schedule a backend `Close S <server_name>` so the PG-side
        //     prepared cache doesn't accumulate orphans.
        //   * Replaced / Inserted: no eviction, no counter bump.
        match self.prepared.cache.put(cache_key, cached) {
            PutOutcome::Evicted(evicted) => {
                self.prepared.anonymous_evictions += 1;
                crate::web::metrics::observe_anonymous_eviction(&self.username, &self.pool_name);
                if log_enabled!(Level::Trace) {
                    trace!(
                        "[{}@{} #c{}] anonymous LRU evict: hash={:#x}, lru_size={}, evicted_total={}, query=\"{}\"",
                        self.username,
                        self.pool_name,
                        self.connection_id,
                        evicted.hash,
                        self.prepared.cache.anonymous_count(),
                        self.prepared.anonymous_evictions,
                        truncate_query_for_log(evicted.parse.query()),
                    );
                }
            }
            PutOutcome::NamedEvicted {
                client_name: evicted_client_name,
                entry: evicted,
            } => {
                let evicted_server_name = evicted.server_name().to_string();
                self.prepared.named_evictions += 1;
                crate::web::metrics::observe_named_eviction(&self.username, &self.pool_name);
                debug!(
                    "[{}@{} #c{}] named cap evict: client_name={:?} server_name={:?} \
                     evicted_total={} (cap={})",
                    self.username,
                    self.pool_name,
                    self.connection_id,
                    evicted_client_name,
                    evicted_server_name,
                    self.prepared.named_evictions,
                    crate::client::core::MAX_NAMED_PREPARED_PER_CLIENT,
                );
                // Schedule backend Close so the PG-side prepared cache
                // doesn't leak the orphan name. Best-effort: if the
                // server doesn't have it (e.g., transaction-pool fan-out
                // means a different backend was used), the deferred close
                // is a no-op on that backend's cache state.
                server.queue_deferred_eviction_close(evicted_server_name);
            }
            // re-Parse with same client name but
            // different query body returns Replaced(prev). The
            // previous server-side name (DOORMAN_N) was NOT
            // closed on the backend - without scheduling the
            // deferred Close, repeated re-Parse cycles
            // accumulated orphaned server-side prepared
            // statements until DEALLOCATE ALL or session
            // restart. When replacement reuses the same backend name
            // (anonymous/same-query re-Parse), closing it would drop
            // the statement the new cache entry still points at.
            PutOutcome::Replaced(prev) => {
                if let Some(evicted_server_name) =
                    replacement_close_target(&prev, server_stmt_name.as_ref())
                {
                    self.prepared.named_evictions += 1;
                    debug!(
                        "[{}@{} #c{}] named replaced (re-Parse with same client \
                         name, different body): evicting server-side {:?}",
                        self.username, self.pool_name, self.connection_id, evicted_server_name,
                    );
                    server.queue_deferred_eviction_close(evicted_server_name);
                } else {
                    trace!(
                        "[{}@{} #c{}] prepared replacement reused backend name {:?}; \
                         not scheduling backend Close",
                        self.username,
                        self.pool_name,
                        self.connection_id,
                        server_stmt_name,
                    );
                }
            }
            _ => {}
        }

        // Update prepared cache stats after modification
        self.update_prepared_cache_stats();

        // Check if server already has this prepared statement
        // For async clients with unique names, this will always be false (new unique name)
        let server_has_it = server.has_prepared_statement(&server_stmt_name);
        if let Some(cache) = pool.prepared_statement_cache.as_ref() {
            // Per-CacheEntry hit/miss for /api/top/prepared. Silent no-op
            // when the entry was evicted between register_parse_to_cache and
            // here — same lock-free policy as /api/top/queries.
            if server_has_it {
                cache.record_hit(hash);
            } else {
                cache.record_miss(hash);
            }
        }
        if server_has_it {
            // For async clients, always send Parse to get real ParseComplete from server
            if self.prepared.async_client {
                debug!(
                    "[{}@{} #c{}] async client: sending Parse `{}` (unique per-session name requires server roundtrip)",
                    self.username, self.pool_name, self.connection_id, server_stmt_name
                );

                // Add parse message to buffer with the server statement name
                let parse_bytes = shared_parse
                    .as_ref()
                    .to_bytes_with_name(&server_stmt_name)?;
                self.buffer.put(&parse_bytes[..]);
            } else {
                // We don't want to send the parse message to the server
                // Track this skipped Parse - ParseComplete will be inserted before BindComplete in response
                debug!(
                    "[{}@{} #c{}] parse skipped for `{}`: already on server pid={}, synthetic ParseComplete queued",
                    self.username, self.pool_name, self.connection_id,
                    server_stmt_name, server.get_process_id()
                );
                // insert_at_beginning starts as false. It will be set to true later
                // if a new Parse is sent to server AFTER this skipped Parse.
                // This ensures correct ordering: ParseComplete for skipped Parse that comes
                // BEFORE new Parse should be at the beginning of the response.
                // has_bind starts as false - will be set to true when Bind is processed.
                crate::client::transaction::enforce_extended_batch_metadata_cap(
                    self.prepared.batch_operations.len(),
                    self.prepared.skipped_parses.len(),
                    1,
                    1,
                    "cached Parse",
                )?;
                self.prepared.skipped_parses.push(SkippedParse {
                    statement_name: server_stmt_name.clone(),
                    target: ParseCompleteTarget::BindComplete,
                    insert_at_beginning: false,
                    has_bind: false,
                });
                // Track operation order for correct ParseComplete insertion
                self.prepared
                    .batch_operations
                    .push(BatchOperation::ParseSkipped {
                        statement_name: server_stmt_name.clone(),
                    });
            }
        } else {
            debug!(
                "[{}@{} #c{}] statement `{}` not in server connection cache, sending Parse to backend",
                self.username, self.pool_name, self.connection_id, server_stmt_name
            );
            // Register to server cache (this may send eviction close to server)
            self.register_parse_to_server_cache(
                false,
                &hash,
                &shared_parse,
                &server_stmt_name,
                pool,
                server,
            )
            .await?;

            // Before sending new Parse, mark pending skipped_parses as insert_at_beginning=true
            // because their ParseComplete should come before the ParseComplete from server.
            // BUT only if they don't have a corresponding Bind yet - if they have Bind,
            // their ParseComplete should be inserted before BindComplete, not at beginning.
            for skipped in &mut self.prepared.skipped_parses {
                if !skipped.insert_at_beginning && !skipped.has_bind {
                    skipped.insert_at_beginning = true;
                }
            }

            // Add parse message to buffer with the server statement name
            let parse_bytes = shared_parse
                .as_ref()
                .to_bytes_with_name(&server_stmt_name)?;
            self.buffer.put(&parse_bytes[..]);

            // Track that we sent a Parse to server in this batch
            self.prepared.parses_sent_in_batch += 1;

            // Track operation order for correct ParseComplete insertion
            self.prepared
                .batch_operations
                .push(BatchOperation::ParseSent {
                    statement_name: server_stmt_name.clone(),
                });
        }

        Ok(())
    }

    async fn write_prepared_error_response(
        &mut self,
        message: &str,
        code: &str,
    ) -> Result<(), Error> {
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        error_response_timeout(&mut self.write, message, code, write_timeout).await
    }

    /// Get lookup key for prepared statement (handles anonymous statements)
    async fn get_prepared_statement_lookup_key(
        &mut self,
        client_given_name: &str,
    ) -> Result<PreparedStatementKey, Error> {
        if client_given_name.is_empty() {
            match self.prepared.last_anonymous_hash {
                Some(hash) => Ok(PreparedStatementKey::Anonymous(hash)),
                None => {
                    if synthetic_miss_should_warn() {
                        warn!(
                            "[{}@{} #c{}] anonymous prepared statement referenced but none registered (suppressing further WARNs for {}s)",
                            self.username,
                            self.pool_name,
                            self.connection_id,
                            SYNTHETIC_MISS_WARN_INTERVAL_MS / 1000,
                        );
                    } else {
                        debug!(
                            "[{}@{} #c{}] anonymous prepared statement referenced but none registered",
                            self.username, self.pool_name, self.connection_id,
                        );
                    }
                    crate::web::metrics::record_synthetic_miss();
                    // SQLSTATE 26000 (invalid_sql_statement_name) matches the
                    // error native PostgreSQL raises for the same condition;
                    // see src/backend/tcop/postgres.c exec_bind_message.
                    self.write_prepared_error_response(
                        "unnamed prepared statement does not exist",
                        "26000",
                    )
                    .await?;
                    Err(Error::ClientError(
                        "Anonymous prepared statement doesn't exist".to_string(),
                    ))
                }
            }
        } else {
            Ok(PreparedStatementKey::Named(client_given_name.to_string()))
        }
    }

    /// Process Bind message immediately without buffering.
    /// Adds data directly to self.buffer.
    pub(crate) async fn process_bind_immediate(
        &mut self,
        message: BytesMut,
        pool: &ConnectionPool,
        server: &mut Server,
    ) -> Result<(), Error> {
        // cap pending pipelined extended-protocol buffer.
        crate::client::transaction::enforce_extended_batch_buffer_cap(
            self.buffer.len(),
            message.len(),
            "Bind",
        )?;
        // Avoid parsing if prepared statements not enabled
        if !self.prepared.enabled {
            debug!(
                "[{}@{} #c{}] prepared statements disabled, forwarding Bind as-is",
                self.username, self.pool_name, self.connection_id,
            );
            if let (Ok(client_given_name), Ok(client_portal_name)) =
                (Bind::get_name(&message), Bind::get_portal_str(&message))
            {
                if let Some(command) = self
                    .prepared
                    .disabled_statement_set_cleanup_commands
                    .get(client_given_name.as_str())
                    .copied()
                {
                    self.prepared
                        .track_portal_set_cleanup_command(client_portal_name, command)?;
                } else if let Some(command) = self
                    .prepared
                    .disabled_statement_reset_cleanup_commands
                    .get(client_given_name.as_str())
                    .copied()
                {
                    self.prepared
                        .track_portal_reset_cleanup_command(client_portal_name, command)?;
                } else {
                    self.prepared
                        .remove_portal_cleanup_command(client_portal_name);
                }
            }
            self.buffer.put(&message[..]);
            // Track operation for correct expected_responses calculation in Flush
            self.prepared.batch_operations.push(BatchOperation::Bind {
                statement_name: Arc::<str>::from(""),
            });
            return Ok(());
        }

        let client_given_name = Bind::get_name(&message)?;
        let client_portal_name = Bind::get_portal_str(&message)?;
        let lookup_key = self
            .get_prepared_statement_lookup_key(&client_given_name)
            .await?;

        let cached = self.prepared.cache.get(&lookup_key).cloned();
        match cached {
            Some(cached) => {
                if cached.intercepted_discard_all && server.in_transaction() {
                    warn!(
                        "[{}@{} #c{}] Bind references intercepted extended-protocol \
                         DISCARD ALL while backend is in a transaction",
                        self.username, self.pool_name, self.connection_id,
                    );
                    self.write_prepared_error_response(
                        "DISCARD ALL cannot run inside a transaction block",
                        "25001",
                    )
                    .await?;
                    return Err(Error::ClientError(
                        "DISCARD ALL cannot run inside a transaction block".to_string(),
                    ));
                }
                // refcount-bumped Arc<str>; downstream clone
                // (into BatchOperation::Bind) is a refcount bump.
                let server_name: Arc<str> = cached.server_name_arc();

                debug!(
                    "[{}@{} #c{}] bind: rewrote statement `{}` -> `{}`",
                    self.username,
                    self.pool_name,
                    self.connection_id,
                    if client_given_name.is_empty() {
                        "<unnamed>"
                    } else {
                        &client_given_name
                    },
                    server_name
                );

                // Ensure prepared statement is on server
                // For async clients, Parse may NOT be in buffer if client reuses cached prepared statement
                // (e.g., asyncpg sends only Bind without Parse for cached statements)
                // Pass the CachedStatement already cloned above so the
                // callee skips a second cache lookup + clone. `lookup_key`
                // is still needed for the pop-on-reject eviction path.
                self.ensure_prepared_statement_is_on_server_cached(
                    &cached, lookup_key, pool, server,
                )
                .await?;

                // Mark the corresponding skipped_parse as having a Bind.
                // This prevents it from being marked as insert_at_beginning when a new Parse arrives,
                // because its ParseComplete should be inserted before BindComplete, not at beginning.
                if let Some(skipped) = self.prepared.skipped_parses.iter_mut().find(|s| {
                    // Arc<str> <-> Arc<str> comparison goes through
                    // str equality on the underlying bytes.
                    s.statement_name.as_ref() == server_name.as_ref()
                        && s.target == ParseCompleteTarget::BindComplete
                        && !s.has_bind
                }) {
                    skipped.has_bind = true;
                }

                if let Some(command) = cached.set_cleanup_command {
                    self.prepared
                        .track_portal_set_cleanup_command(client_portal_name, command)?;
                } else if let Some(command) = cached.reset_cleanup_command {
                    self.prepared
                        .track_portal_reset_cleanup_command(client_portal_name, command)?;
                } else {
                    self.prepared
                        .remove_portal_cleanup_command(client_portal_name);
                }

                let message = Bind::rename(message, &server_name)?;

                // Add directly to buffer after portal-attribution cap checks.
                self.buffer.put(&message[..]);

                // Track operation order for correct ParseComplete insertion
                self.prepared.batch_operations.push(BatchOperation::Bind {
                    statement_name: server_name,
                });

                // /api/top/queries instrumentation. Accept the cache miss /
                // race where the interner entry has been GC'd between intern
                // and Bind — the no-op behaviour in record_query_count is
                // intended to keep the hot path lock-free.
                let is_anonymous = client_given_name.is_empty();
                crate::server::record_query_count(cached.hash, is_anonymous);
                self.prepared.last_bound_for_top = Some((cached.hash, is_anonymous));

                Ok(())
            }
            None => {
                if client_given_name.is_empty() {
                    // Bind "" landed after the anonymous entry was evicted from
                    // the per-client LRU or expired from the interner. Mirror
                    // native PostgreSQL: SQLSTATE 26000 with the canonical
                    // "unnamed prepared statement does not exist" message so
                    // drivers can re-Parse transparently.
                    if synthetic_miss_should_warn() {
                        warn!(
                            "[{}@{} #c{}] Bind \"\" but anonymous prepared no longer cached (suppressing further WARNs for {}s)",
                            self.username,
                            self.pool_name,
                            self.connection_id,
                            SYNTHETIC_MISS_WARN_INTERVAL_MS / 1000,
                        );
                    } else {
                        debug!(
                            "[{}@{} #c{}] Bind \"\" but anonymous prepared no longer cached",
                            self.username, self.pool_name, self.connection_id,
                        );
                    }
                    crate::web::metrics::record_synthetic_miss();
                    self.write_prepared_error_response(
                        "unnamed prepared statement does not exist",
                        "26000",
                    )
                    .await?;
                } else {
                    warn!(
                        "[{}@{} #c{}] Bind references unknown prepared statement {client_given_name:?}",
                        self.username, self.pool_name, self.connection_id,
                    );
                    self.write_prepared_error_response(
                        &format!("prepared statement \"{client_given_name}\" does not exist"),
                        "26000",
                    )
                    .await?;
                }

                Err(Error::ClientError(format!(
                    "Prepared statement `{client_given_name}` doesn't exist"
                )))
            }
        }
    }

    pub(crate) fn track_execute_cleanup_attribution(
        &mut self,
        server: &mut Server,
        message: &BytesMut,
    ) {
        let Some(portal_name) = execute_portal_name(message) else {
            return;
        };
        if let Some(command) = self
            .prepared
            .portal_set_cleanup_commands
            .get(portal_name)
            .copied()
        {
            self.prepared.remove_portal_cleanup_command(portal_name);
            server.track_set_cleanup_commands([command]);
        }
        if let Some(command) = self
            .prepared
            .portal_reset_cleanup_commands
            .get(portal_name)
            .copied()
        {
            self.prepared.remove_portal_cleanup_command(portal_name);
            server.track_reset_cleanup_commands([command]);
        }
    }

    /// Process Describe message immediately without buffering.
    /// Adds data directly to self.buffer.
    pub(crate) async fn process_describe_immediate(
        &mut self,
        message: BytesMut,
        pool: &ConnectionPool,
        server: &mut Server,
    ) -> Result<(), Error> {
        // cap pending pipelined extended-protocol buffer.
        crate::client::transaction::enforce_extended_batch_buffer_cap(
            self.buffer.len(),
            message.len(),
            "Describe",
        )?;
        // Avoid the heavyweight rename/lookup work if prepared statements
        // are disabled, but STILL strict-parse the frame. The previous
        // shape forwarded raw bytes after
        // `*message.get(5).unwrap_or(&b'S')`, sidestepping the
        // trailing-bytes guard and the header bounds check.
        // A malformed Describe (5-byte `D 00 00 00 04`, trailing garbage
        // after the name, invalid UTF-8 in the name) reached the backend
        // verbatim, the backend errored, and the buffer was already
        // committed - split-frame state from a previous Parse/Bind
        // pipeline could end up shipped after the bad Describe. Run the
        // same try_from we run in the enabled path so the strict
        // validation applies uniformly; we just skip the cache lookup.
        if !self.prepared.enabled {
            let describe: Describe = (&message).try_into()?;
            debug!(
                "[{}@{} #c{}] prepared statements disabled, forwarding Describe as-is",
                self.username, self.pool_name, self.connection_id,
            );
            self.buffer.put(&message[..]);
            // Describe message format: 'D' + len(4) + target(1) + name + '\0'
            // target 'S' for statement (2 responses) vs 'P' for portal (1).
            if describe.target == 'P' {
                self.prepared
                    .batch_operations
                    .push(BatchOperation::DescribePortal);
            } else {
                self.prepared
                    .batch_operations
                    .push(BatchOperation::Describe {
                        statement_name: Arc::<str>::from(""),
                    });
            }
            return Ok(());
        }

        let describe: Describe = (&message).try_into()?;
        if describe.target == 'P' {
            debug!(
                "[{}@{} #c{}] describe portal (not statement), passing through",
                self.username, self.pool_name, self.connection_id,
            );
            self.buffer.put(&message[..]);
            // Track portal describe for correct ParseComplete insertion position
            self.prepared
                .batch_operations
                .push(BatchOperation::DescribePortal);
            return Ok(());
        }

        let client_given_name = describe.statement_name.clone();
        let lookup_key = self
            .get_prepared_statement_lookup_key(&client_given_name)
            .await?;

        let cached = self.prepared.cache.get(&lookup_key).cloned();
        match cached {
            Some(cached) => {
                // refcount-bumped Arc<str>; downstream clones (skip
                // re-push, BatchOperation::Describe) are refcount bumps.
                let server_name: Arc<str> = cached.server_name_arc();
                let describe = describe.rename(&server_name);

                debug!(
                    "[{}@{} #c{}] Describe: translated statement name `{}` -> `{}`",
                    self.username,
                    self.pool_name,
                    self.connection_id,
                    client_given_name,
                    describe.statement_name
                );

                // Ensure prepared statement is on server
                // For async clients, Parse may NOT be in buffer if client reuses cached prepared statement
                // (e.g., asyncpg sends only Describe without Parse for cached statements)
                // Reuse the CachedStatement already cloned above to
                // skip a second cache lookup + clone. `lookup_key` is still
                // needed for the pop-on-reject eviction path.
                self.ensure_prepared_statement_is_on_server_cached(
                    &cached, lookup_key, pool, server,
                )
                .await?;

                // If Parse was skipped for this statement, we need to insert ParseComplete
                // before ParameterDescription in the response (not before BindComplete).
                // Find and remove the skipped parse entry, then add a new one with ParameterDescription target.
                // Using position() + remove() + push() instead of iter_mut().find() to avoid issues
                // when multiple Parse operations for the same statement are skipped in a batch.
                if let Some(idx) = self.prepared.skipped_parses.iter().position(|s| {
                    s.statement_name.as_ref() == server_name.as_ref()
                        && s.target == ParseCompleteTarget::BindComplete
                }) {
                    debug!(
                        "[{}@{} #c{}] Describe follows skipped Parse for `{}`, adjusting synthetic ParseComplete position",
                        self.username, self.pool_name, self.connection_id, server_name
                    );
                    let insert_at_beginning = self.prepared.skipped_parses[idx].insert_at_beginning;
                    let has_bind = self.prepared.skipped_parses[idx].has_bind;
                    self.prepared.skipped_parses.remove(idx);
                    self.prepared.skipped_parses.push(SkippedParse {
                        statement_name: server_name.clone(),
                        target: ParseCompleteTarget::ParameterDescription,
                        insert_at_beginning,
                        has_bind,
                    });
                }

                // Add directly to buffer
                let describe_bytes: BytesMut = describe.try_into()?;
                self.buffer.put(&describe_bytes[..]);

                // Track operation order for correct ParseComplete insertion
                self.prepared
                    .batch_operations
                    .push(BatchOperation::Describe {
                        statement_name: server_name,
                    });

                Ok(())
            }

            None => {
                if client_given_name.is_empty() {
                    if synthetic_miss_should_warn() {
                        warn!(
                            "[{}@{} #c{}] Describe \"\" but anonymous prepared no longer cached (suppressing further WARNs for {}s)",
                            self.username,
                            self.pool_name,
                            self.connection_id,
                            SYNTHETIC_MISS_WARN_INTERVAL_MS / 1000,
                        );
                    } else {
                        debug!(
                            "[{}@{} #c{}] Describe \"\" but anonymous prepared no longer cached",
                            self.username, self.pool_name, self.connection_id,
                        );
                    }
                    crate::web::metrics::record_synthetic_miss();
                    self.write_prepared_error_response(
                        "unnamed prepared statement does not exist",
                        "26000",
                    )
                    .await?;
                } else {
                    warn!(
                        "[{}@{} #c{}] Describe references unknown prepared statement `{}`",
                        self.username, self.pool_name, self.connection_id, client_given_name
                    );
                    self.write_prepared_error_response(
                        &format!("prepared statement \"{client_given_name}\" does not exist"),
                        "26000",
                    )
                    .await?;
                }

                Err(Error::ClientError(format!(
                    "Prepared statement `{client_given_name}` doesn't exist"
                )))
            }
        }
    }

    /// Process Close message immediately without buffering.
    /// For prepared statements: removes from the per-client cache.
    /// For others (portal `P` close): adds data directly to self.buffer.
    ///
    /// This function does not increment `pending_close_complete`. The
    /// counter is now always
    /// 0 in this code path, so the related branches in
    /// `execute_server_roundtrip` are inert. The field + reset are
    /// retained as defence-in-depth for a future re-introduction of
    /// the rewrite path.
    ///
    /// Previously rewrote Close to use the cached
    /// backend `DOORMAN_N` server-name. That created a cross-cache
    /// desync - backend dropped DOORMAN_N, but the POOL'S prepared
    /// statement cache (`pool.prepared_statement_cache`) still held
    /// the DOORMAN_N -> Arc<Parse> mapping. The next Parse for the same
    /// query text hit the pool cache and reused DOORMAN_N as the
    /// server-name, while pg_doorman's `ensure_prepared_statement_is_on_server`
    /// check still believed the backend had it (it just dropped it!),
    /// causing the immediate next Bind/Describe to fail with SQLSTATE
    /// 26000 `prepared statement "DOORMAN_N" does not exist`.
    /// (Verified by `batch-parse-describe-bug.feature:138` BDD scenario.)
    ///
    /// The "verbatim Close" behaviour is the acceptable trade-off: the
    /// backend's DOORMAN_N remains cached until the per-server LRU
    /// evicts it. The leak is bounded by `server_prepared_statements_cache_size`.
    /// A future fix should ALSO evict from the pool cache when client
    /// Close happens, then re-enable the rewrite.
    #[inline]
    pub(crate) fn process_close_immediate(&mut self, message: BytesMut) -> Result<(), Error> {
        // cap pending pipelined extended-protocol buffer.
        crate::client::transaction::enforce_extended_batch_buffer_cap(
            self.buffer.len(),
            message.len(),
            "Close",
        )?;
        let close: Close = (&message).try_into()?;

        // Always add Close to buffer in extended query protocol
        // This ensures Close is sent to server when followed by Flush
        self.buffer.put(&message[..]);

        // Track Close operation for correct ParseComplete insertion order
        self.prepared.batch_operations.push(BatchOperation::Close);

        if close.is_portal() {
            self.prepared
                .remove_portal_cleanup_command(close.name.as_str());
        }
        if close.is_prepared_statement() {
            self.prepared
                .remove_disabled_statement_cleanup_command(close.name.as_str());
        }

        // Drop the client-side cache entry immediately. The Close is still
        // forwarded verbatim, matching the bounded server-side tradeoff above,
        // but pg_doorman must not let a later Bind/Describe reuse a statement
        // the client explicitly closed.
        if self.prepared.enabled && close.is_prepared_statement() {
            let key = if close.anonymous() {
                match self.prepared.last_anonymous_hash.take() {
                    Some(hash) => PreparedStatementKey::Anonymous(hash),
                    None => return Ok(()),
                }
            } else {
                PreparedStatementKey::Named(close.name.clone())
            };
            if self.prepared.cache.pop(&key).is_some() {
                // Cache shrunk; refresh client stats so memory/count
                // counters drop accordingly. Without this, SHOW POOLS /
                // Prometheus keep the pre-Close count until the next
                // Parse (driver-side Close-then-reuse pattern leaks
                // stats for the full LRU lifetime).
                self.update_prepared_cache_stats();
            }
        }

        Ok(())
    }

    #[inline]
    pub(crate) fn reset_buffered_state(&mut self) {
        self.buffer.clear();
        self.prepared.pending_close_complete = 0;
        self.prepared.skipped_parses.clear();
        self.prepared.parses_sent_in_batch = 0;
        self.prepared.clear_disabled_statement_cleanup_commands();
    }

    pub(crate) fn drop_rejected_prepared_cache_entries(
        &mut self,
        rejected_server_names: &[String],
    ) -> usize {
        let mut removed = 0usize;
        for server_name in rejected_server_names {
            while let Some((key, _)) = self.prepared.cache.pop_by_server_name(server_name) {
                if let PreparedStatementKey::Anonymous(hash) = key {
                    if self.prepared.last_anonymous_hash == Some(hash) {
                        self.prepared.last_anonymous_hash = None;
                    }
                }
                removed += 1;
            }
        }

        if removed > 0 {
            self.update_prepared_cache_stats();
        }

        removed
    }
}

#[cfg(test)]
mod stats_refresh_invariant_tests {
    /// Guard for the "prepared cache mutated without refreshing
    /// ClientStats" bug class.
    ///
    /// **Invariant:** every direct or helper-routed mutation of
    /// `self.prepared.cache` in the client request path must be followed
    /// (within the same `fn` block) by a call to
    /// `self.update_prepared_cache_stats()`. Without this, `SHOW POOLS`
    /// and the Prometheus prepared-cache gauges keep reporting the
    /// pre-mutation count until the next `Parse` happens to refresh
    /// the snapshot - which can be the full LRU lifetime when a driver
    /// does a `Close`-then-reuse loop without re-Parsing.
    ///
    /// Catches:
    /// - new cache mutation sites added without the refresh,
    /// - regressions that delete the refresh during a future refactor,
    /// - mutation paths added in client/{protocol,transaction}.rs that
    ///   would not have been visible to a single-file scan.
    ///
    /// Scope is function-block rather than ±lines so cosmetic moves of
    /// the refresh inside the same handler don't break the test. Both
    /// the direct `self.prepared.cache.{pop,put,clear}` form and the
    /// indirection through `self.prepared.discard_clear(` (which clears
    /// the cache wholesale) are recognised as mutations.
    #[test]
    fn every_prepared_cache_mutation_refreshes_client_stats() {
        // ALL files in the client request path that touch the prepared
        // cache must be scanned. Adding a new file here is a one-line
        // change; the scan itself is file-agnostic.
        const SOURCES: &[(&str, &str)] = &[
            ("src/client/protocol.rs", include_str!("protocol.rs")),
            ("src/client/transaction.rs", include_str!("transaction.rs")),
        ];
        let mut all_violations: Vec<String> = Vec::new();
        for (file_label, src) in SOURCES {
            all_violations.extend(scan_file_for_unrefreshed_mutations(file_label, src));
        }

        assert!(
            all_violations.is_empty(),
            "Prepared-cache stats-refresh invariant violated:\n{}\n\n\
             Every cache shrink/grow must refresh ClientStats so SHOW \
             POOLS and Prometheus don't keep showing the pre-mutation \
             count. Add `self.update_prepared_cache_stats()` after the \
             mutation (see process_parse_immediate in protocol.rs for \
             the canonical pattern).",
            all_violations.join("\n"),
        );
    }

    /// Per-file scanner: returns violation strings prefixed with the
    /// human-readable file label.
    fn scan_file_for_unrefreshed_mutations(file_label: &str, src: &str) -> Vec<String> {
        const REFRESH_CALL: &str = "self.update_prepared_cache_stats(";
        const MUTATIONS: &[&str] = &[
            "self.prepared.cache.pop(",
            "self.prepared.cache.put(",
            "self.prepared.cache.clear(",
            "self.prepared.discard_clear(",
        ];

        let signatures = [
            "    pub(crate) async fn ",
            "    pub(crate) fn ",
            "    pub async fn ",
            "    pub fn ",
            "    async fn ",
            "    fn ",
        ];
        let mut headers: Vec<(usize, String)> = Vec::new();
        for (idx, line) in src.lines().enumerate() {
            for sig in &signatures {
                if let Some(after) = line.strip_prefix(sig) {
                    let name_end = after.find('(').unwrap_or(after.len());
                    headers.push((idx, after[..name_end].trim().to_string()));
                    break;
                }
            }
        }
        assert!(
            !headers.is_empty(),
            "{file_label}: scanner found zero function headers - the \
             indent/visibility pattern in this file changed in a way \
             that breaks the structural test. Update the `signatures` \
             list in scan_file_for_unrefreshed_mutations."
        );

        let lines: Vec<&str> = src.lines().collect();
        let mut blocks: Vec<(String, String)> = Vec::with_capacity(headers.len());
        for window in headers.windows(2) {
            let (start, name) = (window[0].0, window[0].1.clone());
            let end = window[1].0;
            blocks.push((name, lines[start..end].join("\n")));
        }
        if let Some(last) = headers.last() {
            blocks.push((last.1.clone(), lines[last.0..].join("\n")));
        }

        let mut violations: Vec<String> = Vec::new();
        for (name, body) in &blocks {
            let mutates = MUTATIONS.iter().any(|m| body.contains(m));
            if !mutates {
                continue;
            }
            if !body.contains(REFRESH_CALL) {
                violations.push(format!(
                    "  {file_label}: fn {name}() mutates the prepared cache \
                     without calling self.update_prepared_cache_stats() \
                     anywhere in the same function"
                ));
            }
        }
        violations
    }
}

#[cfg(test)]
mod extended_batch_metadata_cap_tests {
    #[test]
    fn cached_parse_skip_path_enforces_metadata_cap_before_queueing() {
        let src = include_str!("protocol.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let lines: Vec<&str> = impl_src.lines().collect();

        let skip_idx = lines
            .iter()
            .position(|l| l.contains("if server_has_it {"))
            .expect("server_has_it branch not found in Parse handler");
        let queue_rel = lines[skip_idx..]
            .iter()
            .position(|l| l.contains("self.prepared.skipped_parses.push(SkippedParse"))
            .expect("cached Parse skip queue not found");
        let queue_idx = skip_idx + queue_rel;
        let window = lines[skip_idx..queue_idx].join("\n");

        assert!(
            window.contains("enforce_extended_batch_metadata_cap"),
            "cached Parse skip path must cap metadata before queueing SkippedParse; \
             EXTENDED_BATCH_BUFFER_CAP only counts wire bytes and skipped Parse \
             appends no bytes to self.buffer"
        );
    }
}

#[cfg(test)]
mod discard_all_transaction_guard_tests {
    #[test]
    fn discard_all_transaction_guard_rejects_bind_before_backend_send() {
        let src = include_str!("protocol.rs");
        let start = src
            .find("pub(crate) async fn process_bind_immediate")
            .expect("process_bind_immediate not found");
        let end = src[start..]
            .find("    /// Process Describe")
            .map(|idx| start + idx)
            .expect("process_describe_immediate marker not found");
        let body = &src[start..end];
        let some_cached = body
            .find("Some(cached) => {")
            .expect("cached Bind arm not found");
        let body = &body[some_cached..];

        let guard = body
            .find("cached.intercepted_discard_all")
            .expect("Bind must identify cached extended DISCARD ALL rewrites");
        let ensure = body
            .find("ensure_prepared_statement_is_on_server_cached")
            .expect("backend prepare/send path not found");
        let buffer = body
            .find("self.buffer.put(&message[..])")
            .expect("Bind buffer append not found");

        assert!(
            guard < ensure && guard < buffer,
            "cached extended DISCARD ALL must be rejected in an open transaction \
             before touching the backend or queuing Bind bytes"
        );
        assert!(
            body.contains("\"25001\""),
            "transaction-scope DISCARD ALL rejection should use SQLSTATE 25001"
        );
    }

    #[test]
    fn backend_held_prepared_errors_are_deadline_bound() {
        let src = include_str!("protocol.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let helper_start = impl_src
            .find("async fn write_prepared_error_response")
            .expect("prepared error response helper not found");
        let helper_body = &impl_src[helper_start..];
        let helper_end = helper_body
            .find("\n    async fn get_prepared_statement_lookup_key")
            .expect("prepared lookup should follow error helper");
        let helper_body = &helper_body[..helper_end];
        assert!(
            helper_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "prepared synthetic errors must use proxy_copy_data_timeout"
        );
        assert!(
            helper_body.contains("error_response_timeout(&mut self.write"),
            "prepared synthetic errors must use the deadline-bound protocol helper"
        );

        for function_name in [
            "async fn get_prepared_statement_lookup_key",
            "pub(crate) async fn process_bind_immediate",
            "pub(crate) async fn process_describe_immediate",
        ] {
            let start = impl_src
                .find(function_name)
                .unwrap_or_else(|| panic!("{function_name} not found"));
            let body = &impl_src[start..];
            let end = body
                .find("\n    pub(crate) ")
                .or_else(|| body.find("\n    /// "))
                .unwrap_or(body.len());
            let body = &body[..end];
            assert!(
                body.contains("write_prepared_error_response("),
                "{function_name} must use deadline-bound prepared error responses"
            );
            assert!(
                !body
                    .lines()
                    .any(|line| line.trim_start().starts_with("error_response(")),
                "{function_name} must not directly call bare error_response while a backend is checked out"
            );
        }
    }

    #[test]
    fn bind_portal_cleanup_attribution_avoids_pre_reserve_clones() {
        let src = include_str!("protocol.rs");
        let start = src
            .find("pub(crate) async fn process_bind_immediate")
            .expect("process_bind_immediate not found");
        let end = src[start..]
            .find("    pub(crate) fn track_execute_cleanup_attribution")
            .map(|idx| start + idx)
            .expect("track_execute_cleanup_attribution marker not found");
        let body = &src[start..end];

        assert!(
            body.contains("Bind::get_portal_str(&message)"),
            "Bind portal cleanup attribution must borrow portal names before deciding \
             whether they need to be retained"
        );
        assert!(
            !body.contains("client_portal_name.clone()"),
            "Bind portal cleanup attribution must reserve/cap before allocating owned \
             portal-name clones"
        );
        assert!(
            body.contains("track_portal_set_cleanup_command(client_portal_name,")
                && body.contains("track_portal_reset_cleanup_command(client_portal_name,"),
            "tracked Bind portal names must enter cleanup attribution through borrowed APIs"
        );
    }
}

#[cfg(test)]
mod replacement_close_tests {
    use super::replacement_close_target;
    use crate::client::core::CachedStatement;
    use crate::messages::Parse;
    use std::sync::Arc;

    fn cached_with_server_name(name: &str) -> CachedStatement {
        let mut parse = Parse::from_parts("SELECT 1", &[]);
        parse.name = name.to_string();
        CachedStatement {
            parse: Arc::new(parse),
            hash: 0xCAFE,
            intercepted_discard_all: false,
            set_cleanup_command: None,
            reset_cleanup_command: None,
            async_name: None,
        }
    }

    #[test]
    fn replacement_close_is_skipped_for_same_server_name() {
        let previous = cached_with_server_name("DOORMAN_7");
        let replacement = cached_with_server_name("DOORMAN_7");

        assert_eq!(
            replacement_close_target(&previous, replacement.server_name()),
            None
        );
    }

    #[test]
    fn replacement_close_keeps_old_name_when_server_name_changes() {
        let previous = cached_with_server_name("DOORMAN_7");
        let replacement = cached_with_server_name("DOORMAN_8");

        assert_eq!(
            replacement_close_target(&previous, replacement.server_name()).as_deref(),
            Some("DOORMAN_7"),
        );
    }
}

#[cfg(test)]
mod anonymous_close_tests {
    use super::*;
    use crate::client::buffer_pool::PooledBuffer;
    use crate::client::core::PreparedStatementState;
    use crate::messages::Close;
    use crate::pool::PoolIdentifier;
    use crate::server::cleanup::SetCleanupCommand;
    use crate::server::ServerParameters;
    use crate::stats::ClientStats;
    use ahash::RandomState;
    use bytes::BufMut;
    use bytes::BytesMut;
    use dashmap::DashMap;
    use lru::LruCache;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use tokio::io::{empty, sink, BufReader, Empty, Sink};

    fn test_client() -> Client<Empty, Sink> {
        let addr = "127.0.0.1:6543".parse().unwrap();
        Client {
            read: BufReader::new(empty()),
            write: sink(),
            buffer: PooledBuffer::new(),
            addr,
            addr_str: addr.to_string(),
            read_buf: BytesMut::new(),
            connection_id: 1,
            cancel_mode: false,
            transaction_mode: false,
            sql_prepare_session_pinned: false,
            secret_key: 0,
            client_server_map: Arc::new(DashMap::new()),
            stats: Arc::new(ClientStats::default()),
            admin: false,
            last_server_stats: None,
            connected_to_server: false,
            session_xact_start: None,
            pool_name: "db".to_string(),
            username: "user".to_string(),
            cached_pool_id: PoolIdentifier::new("db", "user"),
            migration_pool: None,
            migration_pool_is_dynamic: false,
            server_parameters: ServerParameters::default(),
            prepared: PreparedStatementState::new(true, 0),
            max_memory_usage: u64::MAX,
            client_last_messages_in_tx: PooledBuffer::new(),
            client_pending_begin: None,
            pending_app_name_set: None,
            #[cfg(unix)]
            raw_fd: None,
            #[cfg(all(unix, feature = "tls-migration"))]
            ssl_ptr: None,
        }
    }

    fn cached_with_server_name(name: &str) -> CachedStatement {
        let mut parse = Parse::from_parts("SELECT 1", &[]);
        parse.name = name.to_string();
        CachedStatement {
            parse: Arc::new(parse),
            hash: 0xCAFE,
            intercepted_discard_all: false,
            set_cleanup_command: None,
            reset_cleanup_command: None,
            async_name: None,
        }
    }

    fn make_parse(name: &str, query: &str, param_types: &[i32]) -> BytesMut {
        let body_len = name.len() + 1 + query.len() + 1 + 2 + param_types.len() * 4;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'P');
        buf.put_i32((4 + body_len) as i32);
        buf.put_slice(name.as_bytes());
        buf.put_u8(0);
        buf.put_slice(query.as_bytes());
        buf.put_u8(0);
        buf.put_i16(param_types.len() as i16);
        for oid in param_types {
            buf.put_i32(*oid);
        }
        buf
    }

    fn make_bind(portal: &str, statement: &str) -> BytesMut {
        let body_len = portal.len() + 1 + statement.len() + 1 + 2 + 2 + 2;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'B');
        buf.put_i32((4 + body_len) as i32);
        buf.put_slice(portal.as_bytes());
        buf.put_u8(0);
        buf.put_slice(statement.as_bytes());
        buf.put_u8(0);
        buf.put_i16(0);
        buf.put_i16(0);
        buf.put_i16(0);
        buf
    }

    fn make_execute(portal: &str) -> BytesMut {
        let body_len = portal.len() + 1 + 4;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'E');
        buf.put_i32((4 + body_len) as i32);
        buf.put_slice(portal.as_bytes());
        buf.put_u8(0);
        buf.put_i32(0);
        buf
    }

    #[tokio::test]
    async fn extended_parse_tracks_session_authorization_cleanup() {
        let mut client = test_client();
        let pool = ConnectionPool::test_for_protocol();
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(16).unwrap(),
            RandomState::new(),
        ));
        server
            .prepared_statement_cache
            .as_mut()
            .unwrap()
            .put("DOORMAN_1".to_string(), ());

        let mut parse = Parse::from_parts("SET SESSION AUTHORIZATION app_user", &[]);
        parse.name = "DOORMAN_1".to_string();
        let cached = CachedStatement {
            parse: Arc::new(parse),
            hash: 0x5150,
            intercepted_discard_all: false,
            set_cleanup_command: Some(SetCleanupCommand::SetSessionAuthorization),
            reset_cleanup_command: None,
            async_name: None,
        };
        let _ = client
            .prepared
            .cache
            .put(PreparedStatementKey::Named("stmt_auth".to_string()), cached);

        client
            .process_bind_immediate(make_bind("portal_auth", "stmt_auth"), &pool, &mut server)
            .await
            .expect("Bind must resolve cached SET SESSION AUTHORIZATION statement");
        client.track_execute_cleanup_attribution(&mut server, &make_execute("portal_auth"));

        assert_eq!(
            server.pop_set_cleanup_command(),
            Some(SetCleanupCommand::SetSessionAuthorization),
            "extended SET SESSION AUTHORIZATION must be attributed before CommandComplete(\"SET\")"
        );
    }

    #[tokio::test]
    async fn cleanup_portal_attribution_is_bounded_across_async_flushes() {
        let mut client = test_client();
        let pool = ConnectionPool::test_for_protocol();
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        server.prepared_statement_cache = Some(LruCache::with_hasher(
            NonZeroUsize::new(16).unwrap(),
            RandomState::new(),
        ));
        server
            .prepared_statement_cache
            .as_mut()
            .unwrap()
            .put("DOORMAN_1".to_string(), ());

        let mut parse = Parse::from_parts("SET client.app_user = 'tenant'", &[]);
        parse.name = "DOORMAN_1".to_string();
        let cached = CachedStatement {
            parse: Arc::new(parse),
            hash: 0x5151,
            intercepted_discard_all: false,
            set_cleanup_command: Some(SetCleanupCommand::GenericSet),
            reset_cleanup_command: None,
            async_name: None,
        };
        let _ = client
            .prepared
            .cache
            .put(PreparedStatementKey::Named("stmt_set".to_string()), cached);

        let portal_payload = "p".repeat(1024 * 1024);
        let mut rejected = false;
        for index in 0..24 {
            let portal = format!("{index}_{portal_payload}");
            let result = client
                .process_bind_immediate(make_bind(&portal, "stmt_set"), &pool, &mut server)
                .await;
            client.buffer.clear();
            client.prepared.reset_batch();
            if result.is_err() {
                rejected = true;
                break;
            }
        }

        assert!(
            rejected,
            "SET/RESET portal cleanup attribution must be bounded across async Flush batches"
        );
    }

    #[tokio::test]
    async fn disabled_prepared_unnamed_parse_tracks_session_authorization_cleanup() {
        let mut client = test_client();
        client.prepared.enabled = false;
        let pool = ConnectionPool::test_for_protocol();
        let (mut server, _peer) = crate::server::Server::test_silent_socket();

        client
            .process_parse_immediate(
                make_parse("", "SET SESSION AUTHORIZATION app_user", &[]),
                &pool,
                &mut server,
            )
            .await
            .expect("disabled prepared Parse should be forwarded");
        client
            .process_bind_immediate(make_bind("portal_auth", ""), &pool, &mut server)
            .await
            .expect("disabled prepared Bind should be forwarded");
        client.track_execute_cleanup_attribution(&mut server, &make_execute("portal_auth"));

        assert_eq!(
            server.pop_set_cleanup_command(),
            Some(SetCleanupCommand::SetSessionAuthorization),
            "prepared_statements=false must still attribute unnamed extended SET SESSION AUTHORIZATION"
        );
    }

    #[tokio::test]
    async fn disabled_prepared_rewrites_extended_discard_all_before_forwarding() {
        let mut client = test_client();
        client.transaction_mode = true;
        client.prepared.enabled = false;
        let pool = ConnectionPool::test_for_protocol();
        let (mut server, _peer) = crate::server::Server::test_silent_socket();

        client
            .process_parse_immediate(
                make_parse("discard_stmt", "DISCARD ALL", &[]),
                &pool,
                &mut server,
            )
            .await
            .expect("disabled prepared Parse should be rewritten");

        let forwarded = BytesMut::from(&client.buffer[..]);
        let parse = Parse::try_from(&forwarded).expect("forwarded Parse frame");
        assert_eq!(parse.name, "discard_stmt");
        assert_eq!(parse.query(), EXT_DISCARD_ALL_NOOP);
    }

    #[tokio::test]
    async fn disabled_prepared_cleanup_statement_attribution_is_bounded_across_batches() {
        let mut client = test_client();
        client.prepared.enabled = false;
        let pool = ConnectionPool::test_for_protocol();
        let (mut server, _peer) = crate::server::Server::test_silent_socket();

        let mut rejected = false;
        for index in 0..(crate::client::core::MAX_NAMED_PREPARED_PER_CLIENT + 2) {
            let statement_name = format!("stmt_{index}");
            let result = client
                .process_parse_immediate(
                    make_parse(&statement_name, "SET ROLE audit_reader", &[]),
                    &pool,
                    &mut server,
                )
                .await;
            client.buffer.clear();
            client.prepared.reset_batch();
            if result.is_err() {
                rejected = true;
                break;
            }
        }

        assert!(
            rejected,
            "prepared_statements=false SET/RESET statement attribution must be bounded across batches"
        );
    }

    #[tokio::test]
    async fn disabled_prepared_oversized_parse_tracks_session_authorization_cleanup() {
        let mut client = test_client();
        client.prepared.enabled = false;
        let pool = ConnectionPool::test_for_protocol();
        let (mut server, _peer) = crate::server::Server::test_silent_socket();
        let oversized_query = format!(
            "/*{}*/ SET SESSION AUTHORIZATION app_user",
            "x".repeat(crate::messages::extended::MAX_PARSE_QUERY_BYTES)
        );

        client
            .process_parse_immediate(make_parse("", &oversized_query, &[]), &pool, &mut server)
            .await
            .expect(
                "disabled prepared Parse should be forwarded even when cache Parse would reject it",
            );
        client
            .process_bind_immediate(make_bind("portal_auth", ""), &pool, &mut server)
            .await
            .expect("disabled prepared Bind should be forwarded");
        client.track_execute_cleanup_attribution(&mut server, &make_execute("portal_auth"));

        assert_eq!(
            server.pop_set_cleanup_command(),
            Some(SetCleanupCommand::SetSessionAuthorization),
            "disabled prepared mode must attribute oversized SET SESSION AUTHORIZATION Parse frames"
        );
    }

    #[tokio::test]
    async fn disabled_prepared_unnamed_parse_tracks_set_role_cleanup() {
        let mut client = test_client();
        client.prepared.enabled = false;
        let pool = ConnectionPool::test_for_protocol();
        let (mut server, _peer) = crate::server::Server::test_silent_socket();

        client
            .process_parse_immediate(
                make_parse("", "SET ROLE audit_reader", &[]),
                &pool,
                &mut server,
            )
            .await
            .expect("disabled prepared Parse should be forwarded");
        client
            .process_bind_immediate(make_bind("portal_role", ""), &pool, &mut server)
            .await
            .expect("disabled prepared Bind should be forwarded");
        client.track_execute_cleanup_attribution(&mut server, &make_execute("portal_role"));

        assert_eq!(
            server.pop_set_cleanup_command(),
            Some(SetCleanupCommand::SetRole),
            "prepared_statements=false must still attribute unnamed extended SET ROLE"
        );
    }

    #[test]
    fn close_anonymous_clears_last_anonymous_hash_and_cache_entry() {
        let mut client = test_client();
        let hash = 0xBADC0FFE;
        let _ = client.prepared.cache.put(
            PreparedStatementKey::Anonymous(hash),
            cached_with_server_name("DOORMAN_1"),
        );
        client.prepared.last_anonymous_hash = Some(hash);

        let close: BytesMut = Close::new("").try_into().unwrap();
        client
            .process_close_immediate(close)
            .expect("anonymous Close must parse");

        assert!(
            client.prepared.last_anonymous_hash.is_none(),
            "Close S \"\" must make later Bind \"\" miss like native PostgreSQL"
        );
        assert!(
            client
                .prepared
                .cache
                .get(&PreparedStatementKey::Anonymous(hash))
                .is_none(),
            "anonymous cache entry must not survive explicit Close S \"\""
        );
    }

    #[test]
    fn rejected_parse_rollback_drops_client_cache_entry_and_stats() {
        let mut client = test_client();
        let key_a = PreparedStatementKey::Named("bad_stmt_a".to_string());
        let key_b = PreparedStatementKey::Named("bad_stmt_b".to_string());
        let _ = client
            .prepared
            .cache
            .put(key_a.clone(), cached_with_server_name("DOORMAN_bad"));
        let _ = client
            .prepared
            .cache
            .put(key_b.clone(), cached_with_server_name("DOORMAN_bad"));
        client.update_prepared_cache_stats();

        assert_eq!(client.stats.prepared_named_count(), 2);

        let rejected = vec!["DOORMAN_bad".to_string()];
        let removed = client.drop_rejected_prepared_cache_entries(&rejected);

        assert_eq!(removed, 2);
        assert!(client.prepared.cache.get(&key_a).is_none());
        assert!(client.prepared.cache.get(&key_b).is_none());
        assert_eq!(
            client.stats.prepared_named_count(),
            0,
            "rollback must refresh prepared-cache stats after removing the optimistic Parse"
        );
    }
}
