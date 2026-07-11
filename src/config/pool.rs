//! Connection pool configuration.

use crate::client::util::{
    contains_discard_all_statement, contains_opaque_procedural_statement,
    contains_session_control_statement, contains_session_set_config,
    contains_untrusted_function_call, extract_reset_cleanup_commands, extract_set_cleanup_commands,
};
use crate::errors::Error;
use log::warn;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::fmt;
use std::hash::{Hash, Hasher};

use super::{
    Duration, PoolMode, User, MAX_AUTH_QUERY_WORKERS, MAX_POOL_SIZE,
    MAX_PREPARED_STATEMENTS_CACHE_SIZE,
};

pub(super) fn validate_prewarm_query_does_not_set_session_state(
    scope: &str,
    query: &str,
) -> Result<(), Error> {
    let query_bytes = query.as_bytes();
    if !extract_set_cleanup_commands(query.as_bytes()).is_empty() {
        return Err(Error::BadConfig(format!(
            "{scope}prewarm_query must not contain SET/SET ROLE/SET SESSION AUTHORIZATION; \
             prewarm SQL runs once during backend creation, while transaction-pool cleanup \
             uses RESET ALL/RESET ROLE before reuse and would erase or mis-scope that state. \
             Use startup_parameters for persistent GUCs, or make the application set per-session \
             state explicitly after checkout."
        )));
    }
    if contains_session_set_config(query.as_bytes()) {
        return Err(Error::BadConfig(format!(
            "{scope}prewarm_query must not call session-scoped set_config(...); \
             prewarm SQL runs once during backend creation, so set_config(..., false) \
             or dynamic set_config scope would seed session GUC state before checkout. \
             Use startup_parameters for persistent GUCs, or set per-session state after checkout."
        )));
    }
    if !extract_reset_cleanup_commands(query_bytes).is_empty()
        || contains_discard_all_statement(query_bytes)
        || contains_session_control_statement(query_bytes)
    {
        return Err(Error::BadConfig(format!(
            "{scope}prewarm_query must not contain session state cleanup statements \
             such as RESET, DISCARD ALL, or transaction control; prewarm SQL runs once during backend \
             creation, while pg_doorman mirrors startup parameters for later \
             checkout sync. Use startup_parameters for persistent GUCs, or make \
             the application reset per-session state explicitly after checkout."
        )));
    }
    if contains_opaque_procedural_statement(query_bytes) {
        return Err(Error::BadConfig(format!(
            "{scope}prewarm_query must not contain opaque procedural statements \
             such as DO or CALL; pg_doorman cannot inspect their side effects or \
             mirror arbitrary session state mutations before the backend is reused. \
             Use startup_parameters for persistent GUCs, or make the application \
             set per-session state explicitly after checkout."
        )));
    }
    if contains_untrusted_function_call(query_bytes) {
        return Err(Error::BadConfig(format!(
            "{scope}prewarm_query must not contain untrusted function calls; \
             prewarm SQL runs before the backend is published, and pg_doorman \
             cannot inspect function side effects or mirror arbitrary session \
             state mutations. Use startup_parameters for persistent GUCs, or \
             restrict prewarm SQL to literal reads and proven-local set_config(..., true)."
        )));
    }
    Ok(())
}

pub(super) fn validate_http_url_without_userinfo(scope: &str, url: &str) -> Result<(), Error> {
    if url
        .bytes()
        .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return Err(Error::BadConfig(format!(
            "{scope}: invalid URL; whitespace/control characters are not allowed"
        )));
    }

    let parsed =
        reqwest::Url::parse(url).map_err(|_| Error::BadConfig(format!("{scope}: invalid URL")))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::BadConfig(format!(
            "{scope}: invalid URL scheme; must be http:// or https://"
        )));
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::BadConfig(format!(
            "{scope}: URL userinfo is not allowed; remove username/password from configured URL"
        )));
    }

    if parsed.host_str().is_none() {
        return Err(Error::BadConfig(format!(
            "{scope}: invalid URL; host is required"
        )));
    }

    Ok(())
}

pub(super) fn validate_http_url_without_userinfo_query_fragment(
    scope: &str,
    url: &str,
) -> Result<(), Error> {
    validate_http_url_without_userinfo(scope, url)?;
    let parsed =
        reqwest::Url::parse(url).map_err(|_| Error::BadConfig(format!("{scope}: invalid URL")))?;

    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::BadConfig(format!(
            "{scope}: URL query/fragment is not allowed; configure only scheme, host, port, and path"
        )));
    }

    Ok(())
}

pub(super) fn validate_patroni_api_url(scope: &str, url: &str) -> Result<(), Error> {
    validate_http_url_without_userinfo_query_fragment(scope, url)
}

/// Custom deserializer for users field that supports both formats:
/// - Array format (recommended): `users: [{ username: "user1", ... }]`
/// - Map format (legacy TOML): `users: { "0": { username: "user1", ... } }`
fn deserialize_users<'de, D>(deserializer: D) -> Result<Vec<User>, D::Error>
where
    D: Deserializer<'de>,
{
    struct UsersVisitor;

    impl<'de> Visitor<'de> for UsersVisitor {
        type Value = Vec<User>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a sequence of users or a map with string keys")
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Vec<User>, S::Error>
        where
            S: SeqAccess<'de>,
        {
            let mut users = Vec::new();
            while let Some(user) = seq.next_element()? {
                users.push(user);
            }
            Ok(users)
        }

        fn visit_map<M>(self, mut map: M) -> Result<Vec<User>, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut users = Vec::new();
            while let Some((key, user)) = map.next_entry::<String, User>()? {
                // Validate that key is a valid index (for legacy format)
                if key.parse::<usize>().is_err() {
                    return Err(de::Error::custom(format!(
                        "invalid user key '{key}': expected numeric index or use array format"
                    )));
                }
                users.push(user);
            }
            Ok(users)
        }
    }

    deserializer.deserialize_any(UsersVisitor)
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pool {
    #[serde(default = "Pool::default_pool_mode")]
    pub pool_mode: PoolMode,

    /// Maximum time to allow for establishing a new server connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout: Option<u64>,

    /// Close idle connections that have been opened for longer than this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<u64>,

    /// Close server connections that have been opened for longer than this.
    /// Only applied to idle connections. If the connection is actively used for
    /// longer than this period, the pool will not interrupt it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_lifetime: Option<u64>,

    #[serde(default = "Pool::default_cleanup_server_connections")]
    pub cleanup_server_connections: bool,

    #[serde(default)] // False
    pub log_client_parameter_status_changes: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_name: Option<String>,

    #[serde(default = "Pool::default_server_host")]
    pub server_host: String,

    #[serde(default = "Pool::default_server_port")]
    pub server_port: u16,

    // The real name of the database on the server. If it is not specified, the pool name is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_database: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepared_statements_cache_size: Option<usize>,

    /// Override the per-backend server-level prepared statement LRU
    /// for this pool. When `None`, inherits the value from
    /// `general.server_prepared_statements_cache_size`, which itself
    /// inherits from `prepared_statements_cache_size` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_prepared_statements_cache_size: Option<usize>,

    /// Override global scaling_warm_pool_ratio for this pool (0-100, percentage).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scaling_warm_pool_ratio: Option<u32>,

    /// Override global scaling_fast_retries for this pool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scaling_fast_retries: Option<u32>,

    /// Maximum total server connections to this database across all users.
    /// 0 or None = disabled (default), each user pool works independently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_db_connections: Option<u32>,

    /// Don't evict connections younger than this (milliseconds). Default: 30000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_connection_lifetime: Option<u64>,

    /// Extra connections beyond max_db_connections, used as last resort. Default: 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_pool_size: Option<u32>,

    /// Wait time (milliseconds) before using reserve pool. Default: 3000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_pool_timeout: Option<u64>,

    /// Minimum connections per user protected from coordinator eviction.
    /// Overrides user-level min_pool_size for eviction decisions only
    /// (does not trigger prewarm/replenish). Default: 0 (no protection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_guaranteed_pool_size: Option<u32>,

    /// Patroni REST API endpoints. When the local backend becomes unreachable,
    /// pg_doorman queries `/cluster` to find a live fallback host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patroni_api_urls: Option<Vec<String>>,

    /// How long the local backend stays marked as down after a failed connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_cooldown: Option<Duration>,

    /// HTTP timeout for Patroni API requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patroni_api_timeout: Option<Duration>,

    /// TCP connect timeout for fallback candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_connect_timeout: Option<Duration>,

    /// Lifetime of fallback connections; defaults to `fallback_cooldown` so the
    /// pool returns to the local backend once the cooldown expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_lifetime: Option<Duration>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tls_mode: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tls_ca_cert: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tls_certificate: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tls_private_key: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_query: Option<AuthQueryConfig>,

    /// Pool-level PostgreSQL configuration parameters added to backend
    /// `StartupMessage`s. These values override general settings per key;
    /// passthrough `auth_query` rows can override them per user. Config
    /// load validates reserved keys, GUC names, null bytes, and this
    /// level's size; the merged cascade is checked again before each
    /// backend startup.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub startup_parameters: std::collections::BTreeMap<String, String>,

    /// SQL executed on a backend right before it returns to the pool, to
    /// clear session-scoped state that PostgreSQL itself does not reset
    /// between transactions (advisory locks, session variables, etc.).
    ///
    /// * `None` (omitted) - the iServ-compatible default is used:
    ///   `SELECT pg_catalog.pg_advisory_unlock_all(), public.pgv_free();`.
    ///   Requires the `pg_variables` extension to provide `pgv_free()`.
    /// * `Some("")` - release-query is disabled, nothing is executed.
    /// * `Some("custom SQL")` - exactly that SQL runs on every checkin.
    ///
    /// A SQL error during the release query marks the backend as bad,
    /// so a dirty session is never reused by another client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_query: Option<String>,

    /// SQL executed exactly once on a backend right after it finishes
    /// authentication, before the connection joins the idle pool. Use cases:
    /// warm planner caches, preload libraries, hydrate temp tables consumed
    /// by the workload.
    ///
    /// Do not use `SET` here. Transaction-pool cleanup uses `RESET ALL` /
    /// `RESET ROLE` before reuse and would erase or mis-scope session state
    /// from a one-shot prewarm; config validation rejects `SET`-like prewarm
    /// statements. Use `startup_parameters` for persistent backend GUCs.
    ///
    /// Empty string (the serde-default) means the prewarm is disabled.
    /// A SQL or transport failure marks the backend bad so it never reaches
    /// the idle set and the per-pool `prewarm_failures` counter is bumped.
    ///
    /// User-level `prewarm_query` overrides this when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prewarm_query: String,

    /// When `true` (the default), a standalone `DISCARD ALL` simple-query sent
    /// in **transaction mode** outside of an open transaction is answered
    /// locally with a synthetic `CommandComplete` + `ReadyForQuery` and is
    /// **never forwarded** to PostgreSQL. The per-client prepared-statement
    /// cache is cleared at the same moment so the next `Bind` cannot reach
    /// a backend that does not know the statement.
    ///
    /// Trade-off: real `DISCARD ALL` also performs `UNLISTEN *`, drops
    /// session-temp tables, releases two-phase commits, and clears
    /// session-level sequence/plan caches - none of which pg_doorman
    /// tracks. Set to `false` only when your application **relies on**
    /// those side effects in a transaction-pooled connection (LISTEN/NOTIFY
    /// channels, `CREATE TEMP TABLE ... ON COMMIT DROP`, `PREPARE
    /// TRANSACTION`). The default `true` matches the iServ contract:
    /// no `DISCARD ALL` ever reaches the backend in transaction mode, so
    /// long-lived temp tables and prepared statements survive client
    /// recycling. The interception is in any case bypassed in session
    /// pooling, inside an open transaction, and in COPY mode.
    #[serde(default = "Pool::default_intercept_discard_all")]
    pub intercept_discard_all: bool,

    #[serde(
        default = "Pool::default_users",
        deserialize_with = "deserialize_users"
    )]
    pub users: Vec<User>,
    // Note, don't put simple fields below these configs. There's a compatibility issue with TOML that makes it
    // incompatible to have simple fields in TOML after complex objects. See
    // https://users.rust-lang.org/t/why-toml-to-string-get-error-valueaftertable/85903
}

impl Pool {
    pub fn hash_value(&self) -> u64 {
        let mut s = DefaultHasher::new();
        self.hash(&mut s);
        s.finish()
    }

    pub fn default_pool_mode() -> PoolMode {
        PoolMode::Transaction
    }

    pub fn default_server_port() -> u16 {
        5432
    }

    pub fn default_users() -> Vec<User> {
        Vec::new()
    }
    pub fn default_server_host() -> String {
        String::from("127.0.0.1")
    }

    pub fn default_cleanup_server_connections() -> bool {
        true
    }

    pub fn default_intercept_discard_all() -> bool {
        // Honour the iServ contract: a standalone DISCARD ALL in transaction
        // pooling never reaches PostgreSQL by default. Operators whose
        // application relies on the side effects of real DISCARD ALL
        // (UNLISTEN, dropping session temp tables, ...) must opt out per pool.
        true
    }

    /// Resolve scaling config by merging pool-level overrides with general defaults.
    /// Anticipation/burst params are global-only by design (no per-pool override).
    pub fn resolve_scaling_config(
        &self,
        general: &crate::config::General,
    ) -> crate::pool::ScalingConfig {
        let ratio = self
            .scaling_warm_pool_ratio
            .unwrap_or(general.scaling_warm_pool_ratio);
        let retries = self
            .scaling_fast_retries
            .unwrap_or(general.scaling_fast_retries);
        crate::pool::ScalingConfig {
            warm_pool_ratio: ratio as f32 / 100.0,
            fast_retries: retries,
            max_parallel_creates: general.scaling_max_parallel_creates,
        }
    }

    pub async fn validate(&mut self) -> Result<(), Error> {
        crate::config::startup_parameters::validate(
            &self.startup_parameters,
            "pool.startup_parameters",
        )?;

        if let Some(size) = self.prepared_statements_cache_size {
            if size > MAX_PREPARED_STATEMENTS_CACHE_SIZE {
                return Err(Error::BadConfig(format!(
                    "pool.prepared_statements_cache_size must be <= {MAX_PREPARED_STATEMENTS_CACHE_SIZE}"
                )));
            }
        }
        if let Some(size) = self.server_prepared_statements_cache_size {
            if size > MAX_PREPARED_STATEMENTS_CACHE_SIZE {
                return Err(Error::BadConfig(format!(
                    "pool.server_prepared_statements_cache_size must be <= {MAX_PREPARED_STATEMENTS_CACHE_SIZE}"
                )));
            }
        }

        // Validate release_query: non-empty values must contain at least one
        // non-whitespace character and stay within a sane size limit. The
        // empty-string sentinel ("") disables the release query and is allowed.
        if let Some(ref rq) = self.release_query {
            if !rq.is_empty() {
                if rq.trim().is_empty() {
                    return Err(Error::BadConfig(
                        "release_query contains only whitespace; \
                         use \"\" to disable or omit to use the default"
                            .into(),
                    ));
                }
                if rq.len() > 4096 {
                    return Err(Error::BadConfig(format!(
                        "release_query exceeds maximum length of 4096 bytes \
                         (got {} bytes)",
                        rq.len()
                    )));
                }
                // PostgreSQL simple-query frames terminate at the first NUL
                // byte. A literal `\0` in the configured SQL would cause the
                // backend parser to treat everything after it as a new wire
                // message -> protocol desync -> `mark_bad` on every checkin,
                // silently emptying the pool. Same rationale as the
                // startup_parameters validator at config/startup_parameters.rs.
                if rq.as_bytes().contains(&b'\0') {
                    return Err(Error::BadConfig(
                        "release_query contains a null byte; \
                         PostgreSQL would treat the bytes after it as a new \
                         wire message and the pool would silently mark every \
                         backend bad on checkin"
                            .into(),
                    ));
                }
                if contains_session_set_config(rq.as_bytes()) {
                    return Err(Error::BadConfig(
                        "release_query must not call session-scoped set_config(...); \
                         release SQL runs after client work, and set_config(..., false) \
                         or dynamic set_config scope would seed session GUC state \
                         for the next checkout. Use LOCAL set_config(..., true) or \
                         startup_parameters for persistent GUCs."
                            .into(),
                    ));
                }
                if !extract_set_cleanup_commands(rq.as_bytes()).is_empty()
                    || !extract_reset_cleanup_commands(rq.as_bytes()).is_empty()
                    || contains_discard_all_statement(rq.as_bytes())
                    || contains_session_control_statement(rq.as_bytes())
                {
                    return Err(Error::BadConfig(
                        "release_query must not contain session state cleanup statements \
                         such as SET, RESET, DISCARD ALL, or transaction control; release SQL runs after \
                         client work and pg_doorman cannot mirror arbitrary session \
                         state mutations before the backend is reused. Use \
                         startup_parameters for persistent GUCs or set \
                         release_query = \"\" to disable release SQL."
                            .into(),
                    ));
                }
                if contains_opaque_procedural_statement(rq.as_bytes()) {
                    return Err(Error::BadConfig(
                        "release_query must not contain opaque procedural statements \
                         such as DO or CALL; release SQL runs after client work and \
                         pg_doorman cannot inspect their side effects or mirror arbitrary \
                         session state mutations before the backend is reused. Use \
                         startup_parameters for persistent GUCs or set release_query = \"\" \
                         to disable release SQL."
                            .into(),
                    ));
                }
                if contains_untrusted_function_call(rq.as_bytes()) {
                    return Err(Error::BadConfig(
                        "release_query must not contain untrusted function calls; \
                         release SQL runs after client work and pg_doorman cannot inspect \
                         function side effects or mirror arbitrary session state mutations \
                         before the backend is reused. Use startup_parameters for persistent \
                         GUCs or set release_query = \"\" to disable release SQL."
                            .into(),
                    ));
                }
            }
        }

        // Validate prewarm_query: same shape rules as release_query.
        if !self.prewarm_query.is_empty() {
            if self.prewarm_query.trim().is_empty() {
                return Err(Error::BadConfig(
                    "prewarm_query contains only whitespace; \
                     omit the field to disable prewarm"
                        .into(),
                ));
            }
            if self.prewarm_query.len() > 4096 {
                return Err(Error::BadConfig(format!(
                    "prewarm_query exceeds maximum length of 4096 bytes \
                     (got {} bytes)",
                    self.prewarm_query.len()
                )));
            }
            if self.prewarm_query.as_bytes().contains(&b'\0') {
                return Err(Error::BadConfig(
                    "prewarm_query contains a null byte; \
                     PostgreSQL would treat the bytes after it as a new \
                     wire message and new backends would be marked bad \
                     immediately after startup"
                        .into(),
                ));
            }
            validate_prewarm_query_does_not_set_session_state("", &self.prewarm_query)?;
        }

        // Validate scaling_warm_pool_ratio
        if let Some(ratio) = self.scaling_warm_pool_ratio {
            if ratio > 100 {
                return Err(Error::BadConfig(
                    "scaling_warm_pool_ratio must be 0-100".into(),
                ));
            }
        }

        // Validate pool coordinator settings
        if let Some(max) = self.max_db_connections {
            if max > 0 {
                // `u32::sum()` panics on overflow in
                // debug and wraps in release. Two users with
                // 3_000_000_000 each would silently pass.
                let total_min: u64 = self
                    .users
                    .iter()
                    .filter_map(|u| u.min_pool_size)
                    .map(u64::from)
                    .sum();
                if total_min > u64::from(max) {
                    return Err(Error::BadConfig(format!(
                        "sum of min_pool_size ({total_min}) exceeds max_db_connections ({max}); \
                         not all minimums can be satisfied simultaneously"
                    )));
                }
                if let Some(reserve) = self.reserve_pool_size {
                    if reserve > max {
                        // saturating_add - `max + reserve` could
                        // panic in debug / wrap in release on extreme inputs.
                        log::warn!(
                            "reserve_pool_size ({}) exceeds max_db_connections ({}); \
                             PostgreSQL may receive up to {} connections",
                            reserve,
                            max,
                            max.saturating_add(reserve)
                        );
                    }
                }

                for user in &self.users {
                    if user.pool_size > max {
                        log::warn!(
                            "user '{}' pool_size ({}) exceeds max_db_connections ({}); \
                             effectively capped at {}",
                            user.username,
                            user.pool_size,
                            max,
                            max
                        );
                    }
                }

                // min_connection_lifetime > idle_timeout: eviction will never trigger
                // because idle connections are closed by idle_timeout first.
                if let Some(min_lt) = self.min_connection_lifetime {
                    if let Some(idle) = self.idle_timeout {
                        if min_lt > idle && idle > 0 {
                            log::warn!(
                                "min_connection_lifetime ({min_lt}ms) > idle_timeout ({idle}ms); \
                                 idle connections will be closed before becoming evictable"
                            );
                        }
                    }
                }

                // min_guaranteed_pool_size > any user's pool_size: user becomes
                // immune to eviction but cannot reach the guaranteed minimum.
                if let Some(guaranteed) = self.min_guaranteed_pool_size {
                    if guaranteed > 0 {
                        for user in &self.users {
                            if guaranteed > user.pool_size {
                                warn!(
                                    "min_guaranteed_pool_size ({}) > pool_size ({}) for user '{}'; \
                                     user is immune to eviction but cannot reach the guarantee",
                                    guaranteed,
                                    user.pool_size,
                                    user.username
                                );
                            }
                        }
                    }
                }
            }
        }

        // Validate username uniqueness
        let mut seen_usernames = HashSet::new();
        for user in &self.users {
            if !seen_usernames.insert(&user.username) {
                return Err(Error::BadConfig(format!(
                    "duplicate username '{}' in pool users",
                    user.username
                )));
            }
            user.validate().await?;
        }

        // Validate Patroni-assisted fallback settings
        if let Some(ref urls) = self.patroni_api_urls {
            if urls.is_empty() {
                return Err(Error::BadConfig(
                    "patroni_api_urls cannot be an empty list; \
                     remove the setting to disable Patroni-assisted fallback"
                        .into(),
                ));
            }
            for url in urls {
                validate_patroni_api_url("patroni_api_urls", url)?;
            }
        }

        if let Some(ref dur) = self.fallback_cooldown {
            if dur.as_millis() == 0 {
                return Err(Error::BadConfig("fallback_cooldown must be > 0".into()));
            }
            if dur.as_millis() < 1000 {
                log::warn!(
                    "fallback_cooldown is {}ms (< 1s), \
                     this will cause frequent Patroni API requests; \
                     did you mean \"{}s\"?",
                    dur.as_millis(),
                    dur.as_millis()
                );
            }
        }

        if let Some(ref dur) = self.patroni_api_timeout {
            if dur.as_millis() == 0 {
                return Err(Error::BadConfig("patroni_api_timeout must be > 0".into()));
            }
        }

        if let Some(ref dur) = self.fallback_connect_timeout {
            if dur.as_millis() == 0 {
                return Err(Error::BadConfig(
                    "fallback_connect_timeout must be > 0".into(),
                ));
            }
        }

        if let Some(ref dur) = self.fallback_lifetime {
            if dur.as_millis() == 0 {
                return Err(Error::BadConfig("fallback_lifetime must be > 0".into()));
            }
        }

        // Lifetime longer than the cooldown lets fallback connections outlive
        // the local-backend recovery, mixing primary and fallback in the pool.
        if let (Some(ref lifetime), Some(ref cooldown)) =
            (&self.fallback_lifetime, &self.fallback_cooldown)
        {
            if lifetime.as_millis() > cooldown.as_millis() {
                log::warn!(
                    "fallback_lifetime ({}ms) > fallback_cooldown ({}ms): \
                     fallback connections will coexist with local-backend connections \
                     after the cooldown expires",
                    lifetime.as_millis(),
                    cooldown.as_millis()
                );
            }
        }

        // Validate auth_query config
        if let Some(ref aq) = self.auth_query {
            if aq.query.is_empty() {
                return Err(Error::BadConfig("auth_query.query cannot be empty".into()));
            }
            if aq.user.is_empty() {
                return Err(Error::BadConfig("auth_query.user cannot be empty".into()));
            }
            // server_password without server_user makes no sense;
            // server_user without server_password is valid (PostgreSQL trust auth)
            if aq.server_password.is_some() && aq.server_user.is_none() {
                return Err(Error::BadConfig(
                    "auth_query: server_password requires server_user to be set".into(),
                ));
            }
            if let Some(ref server_user) = aq.server_user {
                if self.users.iter().any(|user| user.username == *server_user) {
                    return Err(Error::BadConfig(format!(
                        "auth_query.server_user '{server_user}' conflicts with a pool user of \
                         the same name; dedicated auth_query shared pools must not alias static \
                         pool users"
                    )));
                }
            }
            if aq.workers == 0 {
                return Err(Error::BadConfig("auth_query.workers must be > 0".into()));
            }
            if aq.workers > MAX_AUTH_QUERY_WORKERS {
                return Err(Error::BadConfig(format!(
                    "auth_query.workers must be <= {MAX_AUTH_QUERY_WORKERS}"
                )));
            }
            // Without this, `aq.pool_size = 0` creates dynamic auth_query pools with
            // `Semaphore::new(0)` - every login hangs for
            // `query_wait_timeout` then errors.
            if aq.pool_size == 0 {
                return Err(Error::BadConfig("auth_query.pool_size must be >= 1".into()));
            }
            if aq.pool_size > MAX_POOL_SIZE {
                return Err(Error::BadConfig(format!(
                    "auth_query.pool_size must be <= {MAX_POOL_SIZE}"
                )));
            }
            if aq.min_pool_size > aq.pool_size {
                return Err(Error::BadConfig(
                    "auth_query: min_pool_size must be <= pool_size".into(),
                ));
            }
        }

        Ok(())
    }
}

impl Default for Pool {
    fn default() -> Pool {
        Pool {
            pool_mode: Self::default_pool_mode(),
            users: Vec::new(),
            server_port: 5432,
            server_host: String::from("127.0.0.1"),
            server_database: None,
            connect_timeout: None,
            idle_timeout: None,
            server_lifetime: None,
            cleanup_server_connections: true,
            log_client_parameter_status_changes: false,
            application_name: None,
            prepared_statements_cache_size: None,
            server_prepared_statements_cache_size: None,
            scaling_warm_pool_ratio: None,
            scaling_fast_retries: None,
            max_db_connections: None,
            min_connection_lifetime: None,
            reserve_pool_size: None,
            reserve_pool_timeout: None,
            min_guaranteed_pool_size: None,
            patroni_api_urls: None,
            fallback_cooldown: None,
            patroni_api_timeout: None,
            fallback_connect_timeout: None,
            fallback_lifetime: None,
            server_tls_mode: None,
            server_tls_ca_cert: None,
            server_tls_certificate: None,
            server_tls_private_key: None,
            auth_query: None,
            startup_parameters: std::collections::BTreeMap::new(),
            release_query: None,
            prewarm_query: String::new(),
            intercept_discard_all: Self::default_intercept_discard_all(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthQueryConfig {
    /// SQL query to fetch credentials. Must return (username, password_hash).
    /// Use $1 for the username parameter.
    pub query: String,

    /// PostgreSQL user for executor connections (runs auth queries).
    pub user: String,

    /// Password for executor user (plaintext). Can be empty for trust mode.
    #[serde(default)]
    pub password: String,

    /// Database for executor connections (default: pool name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,

    /// Number of executor connections (default: 2).
    #[serde(default = "AuthQueryConfig::default_workers")]
    pub workers: u32,

    /// Backend user for data connections. If set, all dynamic users share
    /// one pool with this identity (dedicated mode). If not set, each dynamic
    /// user gets their own pool (passthrough mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_user: Option<String>,

    /// Backend password for dedicated server_user (plaintext).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_password: Option<String>,

    /// Pool size for dynamic user data connections (default: 40).
    #[serde(default = "AuthQueryConfig::default_data_pool_size")]
    pub pool_size: u32,

    /// Minimum connections to maintain per dynamic user pool (default: 0 = no prewarm).
    /// Only applies in passthrough mode (when server_user is not set).
    #[serde(default)]
    pub min_pool_size: u32,

    /// Max cache age for positive entries (default: "1h").
    #[serde(default = "AuthQueryConfig::default_cache_ttl")]
    pub cache_ttl: Duration,

    /// Cache TTL for "user not found" entries (default: "30s").
    #[serde(default = "AuthQueryConfig::default_cache_failure_ttl")]
    pub cache_failure_ttl: Duration,

    /// Min interval between re-fetches for same username on auth failure (default: "1s").
    #[serde(default = "AuthQueryConfig::default_min_interval")]
    pub min_interval: Duration,
}

impl AuthQueryConfig {
    fn default_workers() -> u32 {
        2
    }
    fn default_data_pool_size() -> u32 {
        40
    }
    fn default_cache_ttl() -> Duration {
        Duration::from_hours(1)
    }
    fn default_cache_failure_ttl() -> Duration {
        Duration::from_secs(30)
    }
    fn default_min_interval() -> Duration {
        Duration::from_secs(1)
    }

    /// Returns true if dedicated server_user mode is configured.
    pub fn is_dedicated_mode(&self) -> bool {
        self.server_user.is_some()
    }
}
