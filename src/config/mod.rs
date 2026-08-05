//! Configuration module for the PostgreSQL connection pooler.
//!
//! This module provides configuration parsing, validation, and management
//! for the connection pooler.

use arc_swap::ArcSwap;
use ipnet::IpNet;
use log::{error, info, warn};
use once_cell::sync::Lazy;
use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::{RwLock as TokioRwLock, RwLockReadGuard, RwLockWriteGuard};

use self::tls::TLSMode;
use crate::auth::hba::{AuthMethod, CheckResult, HostType, PgHba};
use crate::errors::Error;
use crate::pool::{ClientServerMap, ConnectionPool};
use crate::transport::ClientTransport;
use crate::utils::format_duration_ms;

// Sub-modules
mod address;
mod byte_size;
mod duration;
mod general;
mod include;
mod pool;
mod pooler_check_query;
pub mod startup_parameters;
mod talos;
pub mod tls;
mod user;
pub mod web;

#[cfg(test)]
mod tests;

// Re-exports
pub use address::{Address, BackendAuthMethod, PoolMode};
pub use byte_size::ByteSize;
pub use duration::Duration;
pub use general::General;
pub use include::{GeneralWithInclude, Include, ServerConfig};
pub use pool::{AuthQueryConfig, Pool};
pub use pooler_check_query::{
    update_pooler_check_query_snapshot, PoolerCheckQuerySnapshot, POOLER_CHECK_QUERY_SNAPSHOT,
};
pub use talos::Talos;
pub use tls::{ServerTlsConfig, ServerTlsMode};
pub use user::User;
pub use web::Web;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const MAX_TLS_RATE_LIMIT_PER_SECOND: usize = 1_000_000;
pub(crate) const MAX_WORKER_THREADS: usize = 1024;
pub(crate) const MAX_CONCURRENT_CREATES: usize = 1024;
pub(crate) const MAX_AUTH_QUERY_WORKERS: u32 = 1024;
pub(crate) const MAX_POOL_SIZE: u32 = 1_000_000;
pub(crate) const MAX_PREPARED_STATEMENTS_CACHE_SIZE: usize = 1_000_000;
/// Lower bound for `general.response_flush_threshold`. Below one page the
/// relay slices a bulk response so finely that the `write()` count per
/// response explodes and the batching stops paying for itself.
pub(crate) const MIN_RESPONSE_FLUSH_THRESHOLD: u64 = 4 * 1024;
/// Upper bound for `general.response_flush_threshold`. The syscall saving is
/// already saturated well before this point, while every backend that served
/// one oversized response keeps twice the threshold resident.
pub(crate) const MAX_RESPONSE_FLUSH_THRESHOLD: u64 = 1024 * 1024;

/// Configuration file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Toml,
    Yaml,
}

impl ConfigFormat {
    /// Detect configuration format from file path extension.
    /// Returns Yaml for .yaml/.yml files, Toml for everything else.
    pub fn detect(path: &str) -> Self {
        let path_lower = path.to_lowercase();
        if path_lower.ends_with(".yaml") || path_lower.ends_with(".yml") {
            ConfigFormat::Yaml
        } else {
            ConfigFormat::Toml
        }
    }
}

/// Parse configuration content based on format.
fn parse_config_content<T: serde::de::DeserializeOwned>(
    contents: &str,
    format: ConfigFormat,
) -> Result<T, Error> {
    warn_on_deprecated_general_keys(contents, format);
    match format {
        ConfigFormat::Toml => toml::from_str(contents)
            .map_err(|err| Error::BadConfig(format!("TOML parse error: {err}"))),
        ConfigFormat::Yaml => serde_yaml::from_str(contents)
            .map_err(|err| Error::BadConfig(format!("YAML parse error: {err}"))),
    }
}

/// Pure helper: returns the deprecated keys present under `general`
/// in the parsed YAML value.
///
/// Each returned `&'static str` is the deprecated field name. New
/// deprecations are added to `DEPRECATED_GENERAL_KEYS` and need no
/// further wiring.
fn find_deprecated_general_keys_yaml(value: &serde_yaml::Value) -> Vec<&'static str> {
    let general = value.get("general").unwrap_or(value);
    let Some(map) = general.as_mapping() else {
        return Vec::new();
    };
    DEPRECATED_GENERAL_KEYS
        .iter()
        .copied()
        .filter(|key| map.contains_key(serde_yaml::Value::String((*key).to_string())))
        .collect()
}

/// Pure helper: returns the deprecated keys present under `general`
/// in the parsed TOML value.
fn find_deprecated_general_keys_toml(value: &toml::Value) -> Vec<&'static str> {
    let general = value.get("general").unwrap_or(value);
    let Some(table) = general.as_table() else {
        return Vec::new();
    };
    DEPRECATED_GENERAL_KEYS
        .iter()
        .copied()
        .filter(|key| table.contains_key(*key))
        .collect()
}

/// Deprecated keys under `[general]`. The corresponding live field
/// must carry `#[serde(alias = "...")]` so the value still flows
/// through; this list only exists to drive the parser-level warning.
const DEPRECATED_GENERAL_KEYS: &[&str] = &["client_prepared_statements_cache_size"];

/// Detect deprecated keys in raw config content and emit a `log::warn!`
/// for each one found. Failures to parse the raw value are silent —
/// the main parser produces the user-facing error.
fn warn_on_deprecated_general_keys(contents: &str, format: ConfigFormat) {
    let deprecated = match format {
        ConfigFormat::Yaml => match serde_yaml::from_str::<serde_yaml::Value>(contents) {
            Ok(value) => find_deprecated_general_keys_yaml(&value),
            Err(_) => return,
        },
        ConfigFormat::Toml => match contents.parse::<toml::Value>() {
            Ok(value) => find_deprecated_general_keys_toml(&value),
            Err(_) => return,
        },
    };
    for key in deprecated {
        match key {
            "client_prepared_statements_cache_size" => warn!(
                "configuration uses deprecated field 'client_prepared_statements_cache_size'; \
                 the value has been mapped to 'client_anonymous_prepared_cache_size' for \
                 backward compatibility. Update your config; the alias may be removed in a \
                 future release."
            ),
            other => warn!(
                "configuration uses deprecated field '{other}'; \
                 update your config — the alias may be removed in a future release."
            ),
        }
    }
}

/// Recursively remove null values from a JSON value.
/// TOML does not support null, so we strip them before conversion.
fn remove_json_nulls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for v in map.values_mut() {
                remove_json_nulls(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                remove_json_nulls(item);
            }
        }
        _ => {}
    }
}

/// Convert configuration content to TOML string for merging.
/// This allows mixing YAML and TOML files in include.files.
fn content_to_toml_string(contents: &str, format: ConfigFormat) -> Result<String, Error> {
    match format {
        ConfigFormat::Toml => Ok(contents.to_string()),
        ConfigFormat::Yaml => {
            // Parse YAML to serde_json::Value as intermediate format
            let mut yaml_value: serde_json::Value = serde_yaml::from_str(contents)
                .map_err(|err| Error::BadConfig(format!("YAML parse error: {err}")))?;
            // Remove null values — TOML does not support them
            remove_json_nulls(&mut yaml_value);
            // Convert JSON value to TOML string
            toml::to_string_pretty(&yaml_value)
                .map_err(|err| Error::BadConfig(format!("YAML to TOML conversion error: {err}")))
        }
    }
}

/// Globally available configuration.
static CONFIG: Lazy<ArcSwap<Config>> = Lazy::new(|| ArcSwap::from_pointee(Config::default()));
static RUNTIME_DEPENDENCY_PUBLISH_LOCK: Lazy<TokioRwLock<()>> = Lazy::new(|| TokioRwLock::new(()));
const GENERATED_ADMIN_PASSWORD_PLACEHOLDER: &str = "change_me_to_a_long_random_secret";
const PUBLISHED_ADMIN_PASSWORDS: &[&str] = &["admin", GENERATED_ADMIN_PASSWORD_PLACEHOLDER];

pub(crate) fn is_published_admin_password(password: &str) -> bool {
    PUBLISHED_ADMIN_PASSWORDS.contains(&password)
}

/// Configuration wrapper.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Config {
    // Serializer maintains the order of fields in the struct
    // so we should always put simple fields before nested fields
    // in all serializable structs to avoid ValueAfterTable errors
    // These errors occur when the toml serializer is about to produce
    // ambiguous toml structure like the one below
    // [main]
    // field1_under_main = 1
    // field2_under_main = 2
    // [main.subconf]
    // field1_under_subconf = 1
    // field3_under_main = 3 # This field will be interpreted as being under subconf and not under main
    #[serde(
        default = "Config::default_path",
        skip_serializing_if = "String::is_empty"
    )]
    pub path: String,

    // General and global settings.
    pub general: General,

    // Web UI / metrics settings.
    #[serde(default = "Web::empty", alias = "prometheus")]
    pub web: Web,

    // Talos settings.
    #[serde(default = "Talos::empty", skip_serializing_if = "Talos::is_empty")]
    pub talos: Talos,

    // Connection pools.
    pub pools: HashMap<String, Pool>,

    // Include files.
    #[serde(
        default = "General::default_include",
        skip_serializing_if = "Include::is_empty"
    )]
    pub include: Include,
}

impl Config {
    pub fn default_path() -> String {
        String::from("pg_doorman.toml")
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            path: Self::default_path(),
            general: General::default(),
            web: Web::empty(),
            pools: HashMap::default(),
            talos: Talos {
                keys: vec![],
                databases: vec![],
                resource_prefixes: vec![],
            },
            include: Include { files: Vec::new() },
        }
    }
}

impl From<&Config> for std::collections::HashMap<String, String> {
    fn from(config: &Config) -> HashMap<String, String> {
        let mut r: Vec<(String, String)> = config
            .pools
            .iter()
            .flat_map(|(pool_name, pool)| {
                [
                    (
                        format!("pools.{pool_name}.pool_mode"),
                        pool.pool_mode.to_string(),
                    ),
                    (
                        format!("pools.{pool_name:?}.users"),
                        pool.users
                            .iter()
                            .map(|user| &user.username)
                            .cloned()
                            .collect::<Vec<String>>()
                            .join(", "),
                    ),
                ]
            })
            .collect();

        let mut static_settings = vec![
            ("host".to_string(), config.general.host.to_string()),
            ("port".to_string(), config.general.port.to_string()),
            (
                "connect_timeout".to_string(),
                config.general.connect_timeout.to_string(),
            ),
            (
                "idle_timeout".to_string(),
                config.general.idle_timeout.to_string(),
            ),
            (
                "shutdown_timeout".to_string(),
                config.general.shutdown_timeout.to_string(),
            ),
        ];

        r.append(&mut static_settings);
        r.iter().cloned().collect()
    }
}

impl Config {
    /// Print current configuration.
    pub fn show(&self) {
        info!("Worker threads: {}", self.general.worker_threads);
        info!(
            "Connection timeout: {}",
            format_duration_ms(self.general.connect_timeout.as_millis())
        );
        info!(
            "Idle timeout: {}",
            format_duration_ms(self.general.idle_timeout.as_millis())
        );
        info!(
            "Log client connections: {}",
            self.general.log_client_connections
        );
        info!(
            "Log client disconnections: {}",
            self.general.log_client_disconnections
        );
        info!(
            "Shutdown timeout: {}",
            format_duration_ms(self.general.shutdown_timeout.as_millis())
        );
        info!(
            "Message size to stream: {}",
            self.general.message_size_to_be_stream
        );
        info!(
            "Response flush threshold: {}",
            self.general.response_flush_threshold
        );
        info!(
            "Max memory usage for processing messages: {}",
            self.general.max_memory_usage
        );
        info!(
            "Default max server lifetime: {}",
            format_duration_ms(self.general.server_lifetime.as_millis())
        );
        info!("Backlog: {}", self.general.backlog);
        info!("Max connections: {}", self.general.max_connections);
        info!("Server round robin: {}", self.general.server_round_robin);
        if self.general.hba.is_empty() {
            if let Some(pg_hba) = &self.general.pg_hba {
                info!("HBA config:\n{pg_hba}\n");
            } else {
                info!("HBA config: empty");
            }
        } else {
            info!("HBA config: {:?} (legacy mode via hba)", self.general.hba);
        }
        match self.general.tls_certificate.clone() {
            Some(tls_certificate) => {
                info!("TLS certificate: {tls_certificate}");

                if let Some(tls_private_key) = self.general.tls_private_key.clone() {
                    info!("TLS private key: {tls_private_key}");
                }
            }
            None => {
                info!("TLS support is disabled");
            }
        };

        info!("server_tls_mode: {}", self.general.server_tls_mode);
        if let Some(ref ca) = self.general.server_tls_ca_cert {
            info!("server_tls_ca_cert: {ca}");
        }
        if let Some(ref cert) = self.general.server_tls_certificate {
            info!("server_tls_certificate: {cert}");
        }

        for (pool_name, pool) in &self.pools {
            info!("[pool: {}] Pool mode: {}", pool_name, pool.pool_mode);
            info!(
                "[pool: {}] Server: {}:{}",
                pool_name, pool.server_host, pool.server_port
            );
            info!(
                "[pool: {}] Cleanup server connections: {}",
                pool_name, pool.cleanup_server_connections
            );
            info!(
                "[pool: {}] Connect timeout: {}",
                pool_name,
                format_duration_ms(
                    pool.connect_timeout
                        .unwrap_or(self.general.connect_timeout.as_millis())
                )
            );
            info!(
                "[pool: {}] Idle timeout: {}",
                pool_name,
                format_duration_ms(
                    pool.idle_timeout
                        .unwrap_or(self.general.idle_timeout.as_millis())
                )
            );
            info!(
                "[pool: {}] Server lifetime: {}",
                pool_name,
                format_duration_ms(
                    pool.server_lifetime
                        .unwrap_or(self.general.server_lifetime.as_millis())
                )
            );
            for (user_index, user) in pool.users.iter().enumerate() {
                info!(
                    "[pool: {}] User {}: {}",
                    pool_name, user_index, user.username
                );
                info!(
                    "[pool: {}] User {} pool size: {}",
                    pool_name, user_index, user.pool_size
                );
            }
        }
    }

    /// Validate the configuration.
    pub async fn validate(&mut self) -> Result<(), Error> {
        // Validate Talos
        self.talos.validate().await?;

        // Validate operator-supplied PostgreSQL startup parameters at the
        // general level; per-pool maps are validated inside `Pool::validate`.
        startup_parameters::validate(
            &self.general.startup_parameters,
            "general.startup_parameters",
        )?;
        // Reject deterministic `general + pool` overflows at config load.
        // For each configured user, mirror the runtime full-packet size
        // check so `pg_doorman -t` fails even when the parameter body fits
        // but `user`/`database`/`application_name` would push the full
        // StartupMessage over `MAX_STARTUP_PACKET_LENGTH`. The checks
        // here only cover size: reserved-key and shape validation has
        // already run per level, and auth_query overlays are still
        // checked at backend startup because they come from PostgreSQL.
        for (pool_name, pool_config) in &self.pools {
            // Same canonical cascade build the runtime does in
            // `ServerPool::new`. Without the canonicalisation here, a
            // pool that overrides `timezone` with `TimeZone` would
            // serialise two rows during validation and disagree with
            // the runtime byte count.
            let merged = startup_parameters::cascade_canonical_keys(&[
                &self.general.startup_parameters,
                &pool_config.startup_parameters,
            ]);
            let merged_size = startup_parameters::serialized_bytes(&merged);
            if merged_size > startup_parameters::MAX_OPERATOR_BUDGET {
                return Err(Error::BadConfig(format!(
                    "merged general + pools.{pool_name}.startup_parameters: serialized \
                     size {merged_size} bytes exceeds operator budget {} (PG \
                     StartupMessage cap is {} bytes; reduce general or pool startup_parameters)",
                    startup_parameters::MAX_OPERATOR_BUDGET,
                    startup_parameters::MAX_STARTUP_PACKET_SIZE,
                )));
            }
            let server_database = pool_config
                .server_database
                .as_deref()
                .unwrap_or(pool_name.as_str());
            if pool_config.server_database.is_some() {
                validate_startup_identity_no_nul(
                    &format!("pools.{pool_name}.server_database"),
                    server_database,
                )?;
            } else {
                validate_startup_identity_no_nul("pool name used as backend database", pool_name)?;
            }
            // Runtime resolves the StartupMessage application_name as
            // pool override → `"pg_doorman"`. Mirror that default so
            // `pg_doorman -t` doesn't accept a config whose only safe
            // case is the empty-string assumption.
            let application_name = pool_config
                .application_name
                .as_deref()
                .unwrap_or("pg_doorman");
            if pool_config.application_name.is_some() {
                validate_startup_identity_no_nul(
                    &format!("pools.{pool_name}.application_name"),
                    application_name,
                )?;
            }
            let validate_user_identity = |display_kind: &str,
                                          display_user: &str,
                                          server_username: &str|
             -> Result<(), Error> {
                let (packet_bytes, _body_bytes) = startup_parameters::packet_and_body_bytes(
                    server_username,
                    server_database,
                    application_name,
                    &merged,
                );
                if packet_bytes > startup_parameters::MAX_STARTUP_PACKET_SIZE {
                    return Err(Error::BadConfig(format!(
                        "merged general + pools.{pool_name}.startup_parameters: full StartupMessage \
                         for {display_kind} '{display_user}' is {packet_bytes} bytes, exceeding \
                         the PG cap of {} bytes (user/database/application_name overhead \
                         included); reduce general or pool startup_parameters",
                        startup_parameters::MAX_STARTUP_PACKET_SIZE,
                    )));
                }
                Ok(())
            };
            for user in &pool_config.users {
                let server_username = user
                    .server_username
                    .as_deref()
                    .unwrap_or(user.username.as_str());
                if user.server_username.is_some() {
                    validate_startup_identity_no_nul(
                        &format!("pools.{pool_name}.users[].server_username"),
                        server_username,
                    )?;
                } else {
                    validate_startup_identity_no_nul(
                        &format!("pools.{pool_name}.users[].username"),
                        &user.username,
                    )?;
                }
                validate_user_identity("user", &user.username, server_username)?;
            }
            // Dedicated auth_query mode opens one shared backend
            // connection identified by `auth_query.server_user`; that
            // identity must fit the packet just like a static user.
            // Use a distinct display kind so operators don't waste time
            // hunting for the name in `pool_config.users`.
            if let Some(aq) = pool_config.auth_query.as_ref() {
                validate_startup_identity_no_nul(
                    &format!("pools.{pool_name}.auth_query.user"),
                    &aq.user,
                )?;
                if let Some(database) = aq.database.as_deref() {
                    validate_startup_identity_no_nul(
                        &format!("pools.{pool_name}.auth_query.database"),
                        database,
                    )?;
                }
                if let Some(shared_user) = aq.server_user.as_deref() {
                    validate_startup_identity_no_nul(
                        &format!("pools.{pool_name}.auth_query.server_user"),
                        shared_user,
                    )?;
                    validate_user_identity("auth_query server_user", shared_user, shared_user)?;
                }
            }
        }

        if self.general.tls_rate_limit_per_second < 100
            && self.general.tls_rate_limit_per_second != 0
        {
            return Err(Error::BadConfig(
                "tls rate limit should be > 100".to_string(),
            ));
        }
        if !self.general.tls_rate_limit_per_second.is_multiple_of(100) {
            return Err(Error::BadConfig(
                "tls rate limit should be multiple 100".to_string(),
            ));
        }
        if self.general.tls_rate_limit_per_second > MAX_TLS_RATE_LIMIT_PER_SECOND {
            return Err(Error::BadConfig(format!(
                "tls rate limit must be <= {MAX_TLS_RATE_LIMIT_PER_SECOND}"
            )));
        }

        // Validate scaling_warm_pool_ratio
        if self.general.scaling_warm_pool_ratio > 100 {
            return Err(Error::BadConfig(
                "general.scaling_warm_pool_ratio must be 0-100".to_string(),
            ));
        }

        // Validate scaling_max_parallel_creates: 0 would deadlock the create path.
        if self.general.scaling_max_parallel_creates == 0 {
            return Err(Error::BadConfig(
                "general.scaling_max_parallel_creates must be >= 1".to_string(),
            ));
        }

        // worker_threads = 0 panics tokio runtime
        // (`worker_threads(0)` asserts val > 0). Fail at validate
        // instead of a cryptic startup panic.
        if self.general.worker_threads == 0 {
            return Err(Error::BadConfig(
                "general.worker_threads must be >= 1".to_string(),
            ));
        }
        if self.general.worker_threads > MAX_WORKER_THREADS {
            return Err(Error::BadConfig(format!(
                "general.worker_threads must be <= {MAX_WORKER_THREADS}"
            )));
        }
        if self.general.message_size_to_be_stream.as_bytes()
            >= crate::messages::MAX_MESSAGE_SIZE as u64
        {
            return Err(Error::BadConfig(format!(
                "general.message_size_to_be_stream must be < {} bytes",
                crate::messages::MAX_MESSAGE_SIZE
            )));
        }
        let response_flush_threshold = self.general.response_flush_threshold.as_bytes();
        if !(MIN_RESPONSE_FLUSH_THRESHOLD..=MAX_RESPONSE_FLUSH_THRESHOLD)
            .contains(&response_flush_threshold)
        {
            return Err(Error::BadConfig(format!(
                "general.response_flush_threshold must be between \
                 {MIN_RESPONSE_FLUSH_THRESHOLD} and {MAX_RESPONSE_FLUSH_THRESHOLD} bytes, \
                 got {response_flush_threshold}"
            )));
        }
        if self.general.max_concurrent_creates == 0 {
            return Err(Error::BadConfig(
                "general.max_concurrent_creates must be >= 1".to_string(),
            ));
        }
        if self.general.max_concurrent_creates > MAX_CONCURRENT_CREATES {
            return Err(Error::BadConfig(format!(
                "general.max_concurrent_creates must be <= {MAX_CONCURRENT_CREATES}"
            )));
        }
        if matches!(self.general.max_blocking_threads, Some(0)) {
            return Err(Error::BadConfig(
                "general.max_blocking_threads must be >= 1 when set".to_string(),
            ));
        }
        if self.general.backlog == 0 && self.general.max_connections > u64::from(u32::MAX) {
            return Err(Error::BadConfig(
                "general.max_connections must be <= u32::MAX when general.backlog is 0".to_string(),
            ));
        }

        let listener_addr = format!("{}:{}", self.general.host, self.general.port);
        match listener_addr.to_socket_addrs() {
            Ok(mut addrs) => {
                if addrs.next().is_none() {
                    return Err(Error::BadConfig(format!(
                        "general.host '{}' with port {} resolved to no listener addresses",
                        self.general.host, self.general.port
                    )));
                }
            }
            Err(err) => {
                return Err(Error::BadConfig(format!(
                    "general.host '{}' with port {} is not a valid listener address: {err}",
                    self.general.host, self.general.port
                )));
            }
        }

        if self.general.admin_username.trim().is_empty() {
            return Err(Error::BadConfig(
                "general.admin_username must not be empty".to_string(),
            ));
        }
        if default_admin_password_exposes_remote_tcp_admin(&self.general) {
            warn!(
                "general.admin_password is a published default or generated placeholder while \
                 general.host listens on a remote-capable TCP address and HBA allows remote TCP \
                 admin access: the virtual admin console is reachable with a well-known password. \
                 Set a unique general.admin_password."
            );
        }

        // tokio_global_queue_interval / tokio_event_interval
        // = 0 panic tokio runtime build (both asserts val > 0).
        // `None` is the safe default; explicit `Some(0)` must be
        // rejected at validate.
        if matches!(self.general.tokio_global_queue_interval, Some(0)) {
            return Err(Error::BadConfig(
                "general.tokio_global_queue_interval must be >= 1 when set".to_string(),
            ));
        }
        if matches!(self.general.tokio_event_interval, Some(0)) {
            return Err(Error::BadConfig(
                "general.tokio_event_interval must be >= 1 when set".to_string(),
            ));
        }

        // zero keepalive timings get rejected by the
        // kernel with EINVAL and the socket runs with kernel
        // defaults - no diagnostic to the operator. Reject up
        // front.
        if self.general.tcp_keepalives_idle == 0 {
            return Err(Error::BadConfig(
                "general.tcp_keepalives_idle must be >= 1 (seconds)".to_string(),
            ));
        }
        if self.general.tcp_keepalives_interval == 0 {
            return Err(Error::BadConfig(
                "general.tcp_keepalives_interval must be >= 1 (seconds)".to_string(),
            ));
        }

        // Validate unix_socket_mode upfront so misconfigurations fail at startup
        // rather than at the moment the listener tries to chmod the socket file.
        General::parse_unix_socket_mode(&self.general.unix_socket_mode)
            .map_err(|err| Error::BadConfig(format!("general.{err}")))?;

        let tcp_socket_buffer_size = self.general.tcp_socket_buffer_size.as_bytes();
        if (1..65_536).contains(&tcp_socket_buffer_size) {
            warn!(
                "general.tcp_socket_buffer_size = {tcp_socket_buffer_size} disables Linux TCP \
                 autotuning with a very small buffer. This can hurt throughput and tail latency \
                 for COPY, wide rows, large result sets, cross-zone traffic, or WAN links. Use at \
                 least 64 KiB unless measurements show a smaller value is safe."
            );
        }

        // Validate mutual exclusion for HBA settings
        if self.general.pg_hba.is_some() && !self.general.hba.is_empty() {
            return Err(Error::BadConfig(
                "general.hba and general.pg_hba cannot be specified at the same time".to_string(),
            ));
        }

        // Legacy general.hba is an IP-based whitelist and has no transport
        // concept, so Unix socket clients unconditionally fall through to
        // Allow in check_hba_with_general. Reject the ambiguous configuration
        // before silently granting access to anyone with filesystem reach.
        if legacy_hba_bypassed_by_unix_socket(&self.general) {
            return Err(Error::BadConfig(
                "general.hba restricts TCP clients by CIDR but does not apply to Unix socket \
                 clients - any local process able to connect to the socket file will bypass the \
                 IP whitelist. Switch to general.pg_hba with explicit `local` rules to cover this \
                 path."
                    .to_string(),
            ));
        }

        // Validate prepared_statements
        if self.general.prepared_statements && self.general.prepared_statements_cache_size == 0 {
            return Err(Error::BadConfig("The value of prepared_statements_cache should be greater than 0 if prepared_statements are enabled".to_string()));
        }
        if self.general.prepared_statements_cache_size > MAX_PREPARED_STATEMENTS_CACHE_SIZE {
            return Err(Error::BadConfig(format!(
                "general.prepared_statements_cache_size must be <= {MAX_PREPARED_STATEMENTS_CACHE_SIZE}"
            )));
        }
        if let Some(size) = self.general.server_prepared_statements_cache_size {
            if size > MAX_PREPARED_STATEMENTS_CACHE_SIZE {
                return Err(Error::BadConfig(format!(
                    "general.server_prepared_statements_cache_size must be <= {MAX_PREPARED_STATEMENTS_CACHE_SIZE}"
                )));
            }
        }
        if let Some(size) = self.general.client_anonymous_prepared_cache_size {
            if size > MAX_PREPARED_STATEMENTS_CACHE_SIZE {
                return Err(Error::BadConfig(format!(
                    "general.client_anonymous_prepared_cache_size must be <= {MAX_PREPARED_STATEMENTS_CACHE_SIZE}"
                )));
            }
        }

        // Validate query interner GC interval. The spawn divides this by 4 to
        // get the sweep tick, so 0 would deadlock the timer.
        if self.general.query_interner_gc_interval_seconds == 0 {
            return Err(Error::BadConfig(
                "general.query_interner_gc_interval_seconds must be > 0".to_string(),
            ));
        }
        if self.general.retain_connections_time.as_millis() == 0 {
            return Err(Error::BadConfig(
                "general.retain_connections_time must be > 0".to_string(),
            ));
        }

        pooler_check_query::validate_pooler_check_query(&self.general.pooler_check_query)?;

        // Loud warning for the foot-gun: 0 is documented as "disable LRU and
        // store anonymous entries in an unbounded map". That's the opposite of
        // pgbouncer convention where 0 typically disables the feature entirely.
        // An operator who sets 0 by reflex from a pgbouncer config gets the
        // unbounded map and a slow memory leak under any driver that mints
        // unique anonymous Parses.
        if matches!(self.general.client_anonymous_prepared_cache_size, Some(0)) {
            warn!(
                "general.client_anonymous_prepared_cache_size = 0 disables the per-client \
                 Anonymous LRU and falls back to an unbounded map. Anonymous prepared \
                 statements will accumulate until the client disconnects; on workloads with \
                 dynamically generated SQL this is a memory leak. Set a positive bound \
                 unless you have specifically chosen the legacy unbounded behaviour."
            );
        }

        // Validate TLS
        {
            if self.general.tls_certificate.is_none() && self.general.tls_private_key.is_some() {
                return Err(Error::BadConfig(
                    "tls_private_key is set but tls_certificate is not".to_string(),
                ));
            }

            if self.general.tls_certificate.is_some() && self.general.tls_private_key.is_none() {
                return Err(Error::BadConfig(
                    "tls_certificate is set but tls_private_key is not".to_string(),
                ));
            }

            if let Some(tls_mode) = self.general.tls_mode.clone() {
                let mode = tls::TLSMode::from_string(tls_mode.as_str())?;
                if (self.general.tls_certificate.is_none()
                    || self.general.tls_private_key.is_none())
                    && (mode != TLSMode::Disable && mode != TLSMode::Allow)
                {
                    return Err(Error::BadConfig(format!(
                        "tls_mode is {mode} but tls_certificate or tls_private_key is not"
                    )));
                }
                if mode == tls::TLSMode::VerifyFull && self.general.tls_ca_cert.is_none() {
                    return Err(Error::BadConfig(format!(
                        "tls_mode is {mode} but tls_ca_cert is not set"
                    )));
                }
                #[cfg(not(target_os = "linux"))]
                if mode == tls::TLSMode::VerifyFull {
                    return Err(Error::BadConfig(
                        "tls_mode verify-full is supported only on linux".to_string(),
                    ));
                }
            }

            if let (Some(tls_certificate), Some(tls_private_key)) = (
                self.general.tls_certificate.clone(),
                self.general.tls_private_key.clone(),
            ) {
                match tls::build_acceptor(
                    Path::new(&tls_certificate),
                    Path::new(&tls_private_key),
                    self.general.tls_ca_cert.as_deref().map(Path::new),
                    self.general.tls_mode.clone(),
                ) {
                    Ok(_) => (),
                    Err(Error::BadConfig(msg)) => {
                        return Err(Error::BadConfig(format!(
                            "tls is incorrectly configured: {msg}"
                        )));
                    }
                    Err(err) => return Err(err),
                }
            };
        }

        // Validate server-facing TLS
        {
            let global_mode = self.general.server_tls_mode.parse::<tls::ServerTlsMode>()?;

            if global_mode.requires_ca() && self.general.server_tls_ca_cert.is_none() {
                return Err(Error::BadConfig(format!(
                    "server_tls_mode is '{global_mode}' but server_tls_ca_cert is not set"
                )));
            }

            match (
                &self.general.server_tls_certificate,
                &self.general.server_tls_private_key,
            ) {
                (Some(_), None) => {
                    return Err(Error::BadConfig(
                        "server_tls_certificate is set but server_tls_private_key is not"
                            .to_string(),
                    ));
                }
                (None, Some(_)) => {
                    return Err(Error::BadConfig(
                        "server_tls_private_key is set but server_tls_certificate is not"
                            .to_string(),
                    ));
                }
                _ => {}
            }

            // Validate that certificate files are readable at startup
            if global_mode != tls::ServerTlsMode::Disable {
                tls::ServerTlsConfig::new(
                    global_mode,
                    self.general.server_tls_ca_cert.as_deref().map(Path::new),
                    self.general
                        .server_tls_certificate
                        .as_deref()
                        .map(Path::new),
                    self.general
                        .server_tls_private_key
                        .as_deref()
                        .map(Path::new),
                )?;
            }

            // Validate per-pool overrides
            for (pool_name, pool_config) in &self.pools {
                let effective_mode = pool_config
                    .server_tls_mode
                    .as_deref()
                    .unwrap_or(&self.general.server_tls_mode);
                let mode = effective_mode.parse::<tls::ServerTlsMode>().map_err(|_| {
                    Error::BadConfig(format!(
                        "pool '{pool_name}': invalid server_tls_mode '{effective_mode}'"
                    ))
                })?;

                let effective_ca = pool_config
                    .server_tls_ca_cert
                    .as_ref()
                    .or(self.general.server_tls_ca_cert.as_ref());
                let effective_cert = pool_config
                    .server_tls_certificate
                    .as_ref()
                    .or(self.general.server_tls_certificate.as_ref());
                let effective_key = pool_config
                    .server_tls_private_key
                    .as_ref()
                    .or(self.general.server_tls_private_key.as_ref());

                if mode.requires_ca() && effective_ca.is_none() {
                    return Err(Error::BadConfig(format!(
                        "pool '{pool_name}': server_tls_mode is '{mode}' but no server_tls_ca_cert"
                    )));
                }

                if pool_config.auth_query.is_some() && mode.requires_tls() {
                    return Err(Error::BadConfig(format!(
                        "pool '{pool_name}': auth_query does not support required server_tls_mode '{mode}'"
                    )));
                }

                match (&effective_cert, &effective_key) {
                    (Some(_), None) => {
                        return Err(Error::BadConfig(format!(
                            "pool '{pool_name}': server_tls_certificate without server_tls_private_key"
                        )));
                    }
                    (None, Some(_)) => {
                        return Err(Error::BadConfig(format!(
                            "pool '{pool_name}': server_tls_private_key without server_tls_certificate"
                        )));
                    }
                    _ => {}
                }
            }
        }

        // Validate general-level Patroni-assisted fallback settings
        if let Some(ref urls) = self.general.patroni_api_urls {
            if urls.is_empty() {
                return Err(Error::BadConfig(
                    "general.patroni_api_urls cannot be an empty list".into(),
                ));
            }
            for url in urls {
                pool::validate_patroni_api_url("general.patroni_api_urls", url)?;
            }
        }

        for pool in self.pools.values_mut() {
            pool.validate().await?;
        }

        // Cross-config validation: coordinator timeouts vs query_wait_timeout
        let qwt = self.general.query_wait_timeout.as_millis();
        for (pool_name, pool_config) in &self.pools {
            if pool_config.max_db_connections.unwrap_or(0) == 0 {
                continue;
            }
            let rpt = pool_config.reserve_pool_timeout.unwrap_or(3000);
            if rpt > qwt {
                log::warn!(
                    "[pool: {pool_name}] reserve_pool_timeout ({rpt}ms) > query_wait_timeout ({qwt}ms); \
                     the outer timeout will fire first, producing a generic Timeout error \
                     instead of the informative DbLimitExhausted error from the coordinator",
                );
            }
        }

        if self.web.log_tap_max_entries > web::MAX_LOG_TAP_ENTRIES {
            return Err(Error::BadConfig(format!(
                "[web].log_tap_max_entries must be <= {} (got {})",
                web::MAX_LOG_TAP_ENTRIES,
                self.web.log_tap_max_entries
            )));
        }

        if self.web.enabled {
            let web_listener_addr = format!("{}:{}", self.web.host, self.web.port);
            web_listener_addr.parse::<SocketAddr>().map_err(|err| {
                Error::BadConfig(format!(
                    "[web].host '{}' with [web].port {} must form a SocketAddr because \
                         the web listener does not resolve DNS names: {err}",
                    self.web.host, self.web.port
                ))
            })?;
        }

        if let Some(ref url) = self.web.sso_proxy_url {
            pool::validate_http_url_without_userinfo_query_fragment("[web].sso_proxy_url", url)?;
        }

        // validate `[web].allowed_admin_origins` entries at
        // config load. extract_authority returns None for malformed
        // entries (scheme-less, embedded whitespace, empty authority)
        // which silently disable CSRF protection for those entries -
        // operators discover it as "all admin POSTs return 403" with
        // no log clue. Reject loudly at startup instead.
        for entry in &self.web.allowed_admin_origins {
            if crate::web::server::csrf::configured_origin_has_userinfo(entry) {
                return Err(Error::BadConfig(
                    "[web].allowed_admin_origins: URL userinfo is not allowed; remove username/password from configured URL".to_string(),
                ));
            }
            if crate::web::server::csrf::configured_origin_has_path_query_fragment(entry) {
                return Err(Error::BadConfig(
                    "[web].allowed_admin_origins: only scheme://host[:port] origins are allowed; remove path, query, and fragment components".to_string(),
                ));
            }
            if crate::web::server::csrf::extract_authority_for_config(entry).is_none() {
                return Err(Error::BadConfig(format!(
                    "[web].allowed_admin_origins entry {entry:?} is not a valid \
                     scheme://host[:port] URL. Examples of valid entries: \
                     \"https://pgd.example:7777\", \"http://admin.local\". \
                     Scheme is required; userinfo, embedded whitespace, and \
                     empty authority are rejected."
                )));
            }
        }

        Ok(())
    }
}

/// Get a read-only instance of the configuration
/// from anywhere in the app.
/// ArcSwap makes this cheap and quick.
pub fn get_config() -> Config {
    (*(*CONFIG.load())).clone()
}

/// Borrow the live `Arc<Config>` without deep-cloning. Use this on
/// hot or warm paths that only need to read a few fields — a tick
/// loop reading one `u64`, a lookup reading one `Pool` — instead of
/// `get_config()`, which clones the whole `Config` (general + every
/// pool + every user). The returned `Arc` is the live snapshot at
/// call time; it does not observe later RELOADs, but that's the
/// usual semantics for a single iteration of a loop.
pub fn config_arc() -> Arc<Config> {
    CONFIG.load_full()
}

async fn load_file(path: &str) -> Result<String, Error> {
    let mut contents = String::new();
    let mut file = match File::open(path).await {
        Ok(file) => file,
        Err(err) => {
            return Err(Error::BadConfig(format!("Could not open '{path}': {err}")));
        }
    };
    match file.read_to_string(&mut contents).await {
        Ok(_) => (),
        Err(err) => {
            return Err(Error::BadConfig(format!(
                "Could not read config file: {err}"
            )));
        }
    };
    Ok(contents)
}

/// Parse and validate the configuration file located at the path without
/// publishing it globally.
///
/// Supports both TOML (.toml) and YAML (.yaml, .yml) formats. Format is
/// auto-detected based on file extension.
async fn parse_config(path: &str) -> Result<Config, Error> {
    let format = ConfigFormat::detect(path);

    // parse only include.files = ["./path/to/file",...]
    let include_only_config_contents = load_file(path).await?;
    let include_config: GeneralWithInclude =
        parse_config_content(&include_only_config_contents, format)?;

    // merge main with include files via serde-toml-merge.
    // Convert to TOML string first (for YAML files), then parse to toml::Value
    let main_toml_str = content_to_toml_string(&include_only_config_contents, format)?;
    let mut config_merged: toml::Value = main_toml_str
        .parse()
        .map_err(|err| Error::BadConfig(format!("Could not parse config file {path}: {err:?}")))?;

    for file in include_config.include.files {
        info!("Merge config with include file: {file}");
        let include_file_content = load_file(file.as_str()).await?;
        let include_format = ConfigFormat::detect(&file);
        // refuse `include.files` inside an
        // included file. Previously, nested `include.files` were
        // silently dropped - `main.toml -> common.toml ->
        // users.toml` shaped configs booted without users.toml,
        // no warning. PG-doorman doesn't support recursive
        // include (would require cycle detection + bounded depth),
        // so make the silent drop loud.
        let nested: GeneralWithInclude =
            parse_config_content(&include_file_content, include_format)?;
        if !nested.include.files.is_empty() {
            return Err(Error::BadConfig(format!(
                "include.files in nested file '{file}' is not supported \
                 ({} entry/ies dropped silently) - flatten the include list \
                 in the root config",
                nested.include.files.len()
            )));
        }
        let include_toml_str = content_to_toml_string(&include_file_content, include_format)?;
        let include_file_value: toml::Value = include_toml_str.parse().map_err(|err| {
            Error::BadConfig(format!("Could not parse include file {file}: {err:?}"))
        })?;
        config_merged = match serde_toml_merge::merge(config_merged, include_file_value) {
            Ok(value) => value,
            Err(err) => {
                return Err(Error::BadConfig(format!(
                    "Could not merge config file {file}: {err:?}"
                )));
            }
        };
    }

    // typed BadConfig instead of unwrap panic.
    let table = config_merged.as_table().ok_or_else(|| {
        Error::BadConfig(format!(
            "merged config root is not a TOML table: {config_merged:?}"
        ))
    })?;
    let mut config: Config = match toml::from_str(&table.to_string()) {
        Ok(config) => config,
        Err(err) => {
            return Err(Error::BadConfig(format!("Could not merge config: {err:?}")));
        }
    };

    config.validate().await?;

    config.path = path.to_string();

    Ok(config)
}

pub(crate) async fn parse_unpublished_config(path: &str) -> Result<Config, Error> {
    parse_config(path).await
}

fn jwt_key_files(config: &Config) -> Vec<String> {
    config
        .pools
        .values()
        .flat_map(|pool| pool.users.iter())
        .filter_map(|user| {
            crate::auth::jwt::parse_jwt_pub_key_password(&user.password)
                .ok()
                .flatten()
                .map(|jwt| jwt.key_filename)
        })
        .collect()
}

pub(crate) struct StagedConfigRuntimeDependencies {
    talos: crate::auth::talos::StagedTalosPubKeys,
    jwt: crate::auth::jwt::StagedJwtPubKeys,
}

pub(crate) struct RuntimeDependencyPublishGuards {
    _publish: RwLockWriteGuard<'static, ()>,
    talos: crate::auth::talos::TalosPubKeysWriteGuard,
    jwt: crate::auth::jwt::JwtPubKeysWriteGuards,
}

pub(crate) async fn runtime_dependency_publish_read_guard() -> RwLockReadGuard<'static, ()> {
    RUNTIME_DEPENDENCY_PUBLISH_LOCK.read().await
}

pub(crate) async fn runtime_dependency_publish_guards() -> RuntimeDependencyPublishGuards {
    let publish = RUNTIME_DEPENDENCY_PUBLISH_LOCK.write().await;
    let talos = crate::auth::talos::talos_pub_keys_write_guard().await;
    let jwt = crate::auth::jwt::jwt_pub_keys_write_guards().await;
    RuntimeDependencyPublishGuards {
        _publish: publish,
        talos,
        jwt,
    }
}

pub(crate) fn stage_config_runtime_dependencies(
    config: &Config,
) -> Result<StagedConfigRuntimeDependencies, Error> {
    Ok(StagedConfigRuntimeDependencies {
        talos: crate::auth::talos::stage_talos_pub_keys(&config.talos.keys)?,
        jwt: crate::auth::jwt::stage_jwt_pub_keys(jwt_key_files(config))?,
    })
}

pub(crate) fn publish_staged_config_runtime_dependencies(
    staged: StagedConfigRuntimeDependencies,
    guards: &mut RuntimeDependencyPublishGuards,
) {
    crate::auth::talos::publish_staged_talos_pub_keys_locked(staged.talos, &mut guards.talos);
    crate::auth::jwt::publish_staged_jwt_pub_keys_locked(staged.jwt, &mut guards.jwt);
}

pub(crate) fn publish_config_snapshot(config: Config) {
    CONFIG.store(Arc::new(config.clone()));
    update_pooler_check_query_snapshot(&config.general.pooler_check_query);
}

pub(crate) async fn publish_config(config: Config) -> Result<(), Error> {
    let staged = stage_config_runtime_dependencies(&config)?;
    let mut guards = runtime_dependency_publish_guards().await;
    publish_config_snapshot(config);
    publish_staged_config_runtime_dependencies(staged, &mut guards);
    Ok(())
}

fn apply_general_log_level(config: &Config) {
    if let Some(level) = config.general.log_level.as_deref() {
        if let Err(err) = crate::app::log_level::set_log_level(level) {
            warn!(
                "[general] log_level = {level:?} failed to apply on reload: {err}; \
                 keeping current runtime filter"
            );
        }
    }
}

fn validate_startup_identity_no_nul(field: &str, value: &str) -> Result<(), Error> {
    if value.as_bytes().contains(&b'\0') {
        return Err(Error::BadConfig(format!(
            "{field} contains NUL byte; backend StartupMessage keys and values are NUL-terminated"
        )));
    }
    Ok(())
}

/// Parse the configuration file located at the path and publish it globally.
pub async fn parse(path: &str) -> Result<(), Error> {
    let config = parse_config(path).await?;
    publish_config(config).await?;
    Ok(())
}

/// true when any client-facing TLS field differs
/// between two `General` snapshots. SIGHUP RELOAD cannot rebuild the
/// running `TlsAcceptor`; the operator must restart to pick up new
/// certificates. Exposed as a free function so the comparison can be
/// covered by a unit test without spinning up a config-reload harness.
pub(crate) fn client_facing_tls_fields_differ(old: &General, new: &General) -> bool {
    old.tls_certificate != new.tls_certificate
        || old.tls_private_key != new.tls_private_key
        || old.tls_ca_cert != new.tls_ca_cert
        || old.tls_mode != new.tls_mode
        || old.tls_rate_limit_per_second != new.tls_rate_limit_per_second
}

/// Connection-lifecycle timeouts a pool freezes at construction time and
/// that a reload DOES apply, by rebuilding every pool that inherits them.
///
/// These are folded into the reload reuse fingerprint
/// (`pool::static_pool_fingerprint`), so a changed value here means the
/// running pool is replaced instead of reused. Naming them in the log is
/// what tells the operator why the pools were recreated. The two lists are
/// kept in sync by
/// `pool::tests::pool_rebuild_general_fields_changed_matches_the_fingerprint`.
pub(crate) fn pool_rebuild_general_fields_changed(
    old: &General,
    new: &General,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if old.idle_timeout != new.idle_timeout {
        fields.push("general.idle_timeout");
    }
    if old.server_lifetime != new.server_lifetime {
        fields.push("general.server_lifetime");
    }
    if old.server_idle_check_timeout != new.server_idle_check_timeout {
        fields.push("general.server_idle_check_timeout");
    }
    if old.query_wait_timeout != new.query_wait_timeout {
        fields.push("general.query_wait_timeout");
    }
    if old.connect_timeout != new.connect_timeout {
        fields.push("general.connect_timeout");
    }
    fields
}

/// `general` values a pool ALSO freezes at construction time, but which a
/// reload deliberately does NOT apply.
///
/// Applying them would mean rebuilding every pool, and a rebuild costs a
/// cold prepared-statement cache plus a burst of re-`Parse` against
/// PostgreSQL. That price is accepted for the connection-lifecycle timeouts
/// above (rarely and deliberately edited, and not applying them is an
/// operational trap), and refused for cache sizing and failover tuning.
///
/// A pool therefore keeps serving the old value until the process restarts,
/// which is exactly what the historical `config unchanged` line hid from
/// operators: this list is logged as a warning instead. Kept disjoint from
/// [`pool_rebuild_general_fields_changed`] and pinned against the
/// fingerprint by
/// `pool::tests::restart_only_general_pool_fields_do_not_rebuild_pools`.
pub(crate) fn restart_only_general_pool_fields_changed(
    old: &General,
    new: &General,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if old.max_concurrent_creates != new.max_concurrent_creates {
        fields.push("general.max_concurrent_creates");
    }
    if old.server_round_robin != new.server_round_robin {
        fields.push("general.server_round_robin");
    }
    if old.scaling_warm_pool_ratio != new.scaling_warm_pool_ratio {
        fields.push("general.scaling_warm_pool_ratio");
    }
    if old.scaling_fast_retries != new.scaling_fast_retries {
        fields.push("general.scaling_fast_retries");
    }
    if old.scaling_max_parallel_creates != new.scaling_max_parallel_creates {
        fields.push("general.scaling_max_parallel_creates");
    }
    if old.prepared_statements != new.prepared_statements {
        fields.push("general.prepared_statements");
    }
    if old.prepared_statements_cache_size != new.prepared_statements_cache_size {
        fields.push("general.prepared_statements_cache_size");
    }
    if old.server_prepared_statements_cache_size != new.server_prepared_statements_cache_size {
        fields.push("general.server_prepared_statements_cache_size");
    }
    if old.patroni_api_urls != new.patroni_api_urls {
        fields.push("general.patroni_api_urls");
    }
    if old.fallback_cooldown != new.fallback_cooldown {
        fields.push("general.fallback_cooldown");
    }
    if old.patroni_api_timeout != new.patroni_api_timeout {
        fields.push("general.patroni_api_timeout");
    }
    if old.fallback_connect_timeout != new.fallback_connect_timeout {
        fields.push("general.fallback_connect_timeout");
    }
    if old.fallback_lifetime != new.fallback_lifetime {
        fields.push("general.fallback_lifetime");
    }
    fields
}

pub(crate) fn restart_only_listener_fields_changed(
    old: &Config,
    new: &Config,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if old.general.host != new.general.host {
        fields.push("general.host");
    }
    if old.general.port != new.general.port {
        fields.push("general.port");
    }
    if old.general.unix_socket_dir != new.general.unix_socket_dir {
        fields.push("general.unix_socket_dir");
    }
    if old.general.unix_socket_mode != new.general.unix_socket_mode {
        fields.push("general.unix_socket_mode");
    }
    if old.general.backlog != new.general.backlog {
        fields.push("general.backlog");
    }
    if old.general.tls_certificate != new.general.tls_certificate {
        fields.push("general.tls_certificate");
    }
    if old.general.tls_private_key != new.general.tls_private_key {
        fields.push("general.tls_private_key");
    }
    if old.general.tls_ca_cert != new.general.tls_ca_cert {
        fields.push("general.tls_ca_cert");
    }
    if old.general.tls_mode != new.general.tls_mode {
        fields.push("general.tls_mode");
    }
    if old.general.tls_rate_limit_per_second != new.general.tls_rate_limit_per_second {
        fields.push("general.tls_rate_limit_per_second");
    }
    if old.general.worker_threads != new.general.worker_threads {
        fields.push("general.worker_threads");
    }
    if old.general.worker_cpu_affinity_pinning != new.general.worker_cpu_affinity_pinning {
        fields.push("general.worker_cpu_affinity_pinning");
    }
    if old.general.worker_stack_size != new.general.worker_stack_size {
        fields.push("general.worker_stack_size");
    }
    if old.general.max_blocking_threads != new.general.max_blocking_threads {
        fields.push("general.max_blocking_threads");
    }
    if old.general.tokio_global_queue_interval != new.general.tokio_global_queue_interval {
        fields.push("general.tokio_global_queue_interval");
    }
    if old.general.tokio_event_interval != new.general.tokio_event_interval {
        fields.push("general.tokio_event_interval");
    }
    if old.general.query_interner_gc_interval_seconds
        != new.general.query_interner_gc_interval_seconds
    {
        fields.push("general.query_interner_gc_interval_seconds");
    }
    if old.general.retain_connections_time != new.general.retain_connections_time {
        fields.push("general.retain_connections_time");
    }
    if old.general.retain_connections_max != new.general.retain_connections_max {
        fields.push("general.retain_connections_max");
    }
    if old.web.enabled != new.web.enabled {
        fields.push("web.enabled");
    }
    if old.web.host != new.web.host {
        fields.push("web.host");
    }
    if old.web.port != new.web.port {
        fields.push("web.port");
    }
    fields
}

pub async fn reload_config(client_server_map: ClientServerMap) -> Result<bool, Error> {
    let old_config = get_config();

    let new_config = match parse_config(&old_config.path).await {
        Ok(config) => config,
        Err(err) => {
            error!("Config reload error: {err}");
            return Err(Error::BadConfig(format!("Config reload error: {err:?}")));
        }
    };

    let restart_only_fields = restart_only_listener_fields_changed(&old_config, &new_config);
    if !restart_only_fields.is_empty() {
        let fields = restart_only_fields.join(", ");
        let msg = format!(
            "Config reload rejected: {fields} require a process restart; \
             live runtime components keep using the old values"
        );
        error!("{msg}");
        return Err(Error::BadConfig(msg));
    }

    if old_config != new_config {
        info!("Config changed, reloading");

        // SIGHUP RELOAD cannot rebuild the running
        // TlsAcceptor - `init_tls(&config)` is called exactly once in
        // `app::server::run_main()` and the acceptor is cloned into each
        // accept task at startup. The reload path swaps pools, log
        // levels, HBA, etc., but client-facing TLS material (cert, key,
        // CA, mode, rate limit) keeps using the ORIGINAL acceptor until
        // the process restarts. The legacy code logged every reload as
        // "Config changed, reloading" + the new cert path, reinforcing
        // a false belief among operators that an ACME / Let's Encrypt
        // rotation pipeline had taken effect. Emit a typed warning when
        // any TLS-related field actually differs so the operator knows
        // a restart is still needed.
        if client_facing_tls_fields_differ(&old_config.general, &new_config.general) {
            warn!(
                "RELOAD: client-facing TLS fields changed but the running TlsAcceptor \
                 cannot be hot-reloaded - new TLS handshakes still use the OLD \
                 certificate/key/CA/mode/rate. Restart the process to pick up the \
                 new values. (tls_certificate, tls_private_key, tls_ca_cert, \
                 tls_mode, tls_rate_limit_per_second)"
            );
        }

        // A pool freezes its connection-lifecycle timeouts when it is built,
        // so the reload recreates every pool that inherits an edited one -
        // silently reusing the running pool would keep the old value while
        // logging `config unchanged`. Name the fields so the operator can
        // tell why the pools were rebuilt (and why PostgreSQL carries two
        // generations of backends until the old sessions disconnect).
        let rebuild_fields =
            pool_rebuild_general_fields_changed(&old_config.general, &new_config.general);
        if !rebuild_fields.is_empty() {
            let fields = rebuild_fields.join(", ");
            info!(
                "RELOAD: {fields} changed - recreating every pool that inherits \
                 them. Sessions still holding the previous generation keep it \
                 until they disconnect, so it drains in the background instead of \
                 being closed under them."
            );

            // ... every pool EXCEPT the auth_query ones. Their shared and
            // dynamic pools are reused across the reload and froze the old
            // timeouts, so the operator has to be told the edit did not
            // reach them - this is the `config unchanged` trap again, one
            // level down.
            let pinned = crate::pool::auth_query_pools_pinned_to_old_lifecycle_timeouts(
                &old_config,
                &new_config,
            );
            if !pinned.is_empty() {
                let pools = pinned.join(", ");
                warn!(
                    "RELOAD: {fields} changed but the auth_query pools ({pools}) keep \
                     the OLD values - their dedicated shared pool and their passthrough \
                     dynamic pools survive the reload and froze the timeouts when they \
                     were built. Restart pg_doorman to apply the new values there. They \
                     are deliberately not recreated: a passthrough session cannot \
                     survive its dynamic pool being replaced, so applying a timeout \
                     edit this way would disconnect every client of those pools."
                );
            }
        }

        // The rest of what a pool freezes is deliberately NOT applied: a
        // rebuild would cost a cold prepared-statement cache and a burst of
        // re-Parse on a live workload. Say so out loud - this is the case
        // the old `config unchanged` line hid, leaving operators believing
        // the whole file had taken effect.
        let restart_only_pool_fields =
            restart_only_general_pool_fields_changed(&old_config.general, &new_config.general);
        if !restart_only_pool_fields.is_empty() {
            let fields = restart_only_pool_fields.join(", ");
            warn!(
                "RELOAD: {fields} changed but are NOT applied by a reload - every \
                 pool froze them when it was built and keeps serving the old \
                 values. Restart pg_doorman to pick them up."
            );
        }

        ConnectionPool::from_config_snapshot(client_server_map, new_config.clone()).await?;
        apply_general_log_level(&new_config);
        // Refresh the web listener's reload-aware options only after the
        // runtime pool apply succeeds, so failed reloads keep admin/API
        // state aligned with the still-running pools.
        crate::web::refresh_options_from_config();

        // Refresh static info gauges so disappeared pools and new
        // (user, database, pool_mode) triples are reflected in
        // /metrics on this same scrape.
        crate::web::metrics::refresh_static_info_metrics();
        Ok(true)
    } else {
        let staged = stage_config_runtime_dependencies(&new_config)?;
        let mut guards = runtime_dependency_publish_guards().await;
        publish_staged_config_runtime_dependencies(staged, &mut guards);
        apply_general_log_level(&new_config);
        crate::web::refresh_options_from_config();
        crate::web::metrics::refresh_static_info_metrics();
        Ok(false)
    }
}

pub fn check_hba(
    transport: &ClientTransport,
    type_auth: &str,
    username: &str,
    database: &str,
) -> CheckResult {
    // Per-connection auth path: borrow the live config snapshot instead of
    // deep-cloning the whole Config just to read &general for HBA matching.
    let config = CONFIG.load();
    check_hba_with_general(&config.general, transport, type_auth, username, database)
}

/// True when the operator enabled a Unix listener alongside the legacy
/// IP-based `general.hba` whitelist, without a `pg_hba` snippet to cover
/// the `local` transport. In this shape Unix clients bypass the CIDR
/// check entirely — see `check_hba_with_general`.
pub(crate) fn legacy_hba_bypassed_by_unix_socket(general: &General) -> bool {
    general.unix_socket_dir.is_some() && general.pg_hba.is_none() && !general.hba.is_empty()
}

fn parse_listener_host_ip(host: &str) -> Option<IpAddr> {
    host.parse::<IpAddr>().ok().or_else(|| {
        host.strip_prefix('[')
            .and_then(|stripped| stripped.strip_suffix(']'))
            .and_then(|stripped| stripped.parse::<IpAddr>().ok())
    })
}

fn listener_may_accept_remote_tcp(general: &General) -> bool {
    let listener_addr = format!("{}:{}", general.host, general.port);
    listener_addr
        .to_socket_addrs()
        .map(|mut addrs| addrs.any(|addr| !addr.ip().is_loopback()))
        .unwrap_or_else(|_| {
            parse_listener_host_ip(&general.host)
                .map(|ip| !ip.is_loopback())
                .unwrap_or(false)
        })
}

fn hba_net_may_include_remote_tcp_peer(net: &IpNet) -> bool {
    if net.addr().is_unspecified() && net.addr() == net.network() && net.addr() == net.broadcast() {
        return false;
    }
    !(net.network().is_loopback() && net.broadcast().is_loopback())
}

fn legacy_hba_may_allow_remote_tcp_admin(general: &General) -> bool {
    general.hba.is_empty() || general.hba.iter().any(hba_net_may_include_remote_tcp_peer)
}

fn admin_hba_method_allows_password(method: &AuthMethod) -> bool {
    matches!(
        method,
        AuthMethod::Trust | AuthMethod::Md5 | AuthMethod::ScramSha256
    )
}

fn pg_hba_may_allow_remote_tcp_admin(pg_hba: &PgHba, admin_username: &str) -> bool {
    pg_hba.rules.iter().any(|rule| {
        matches!(
            rule.host_type,
            HostType::Host | HostType::HostSSL | HostType::HostNoSSL
        ) && rule
            .address
            .as_ref()
            .map(hba_net_may_include_remote_tcp_peer)
            .unwrap_or(false)
            && ["pgdoorman", "pgbouncer"]
                .iter()
                .any(|database| rule.database.matches(database))
            && rule.user.matches(admin_username)
            && admin_hba_method_allows_password(&rule.method)
    })
}

fn default_admin_password_exposes_remote_tcp_admin(general: &General) -> bool {
    if !is_published_admin_password(&general.admin_password)
        || !listener_may_accept_remote_tcp(general)
    {
        return false;
    }

    if let Some(pg_hba) = general.pg_hba.as_ref() {
        return pg_hba_may_allow_remote_tcp_admin(pg_hba, &general.admin_username);
    }

    legacy_hba_may_allow_remote_tcp_admin(general)
}

/// Pure evaluation of HBA rules against an explicit [`General`] snapshot.
///
/// Split out of [`check_hba`] so that unit tests can exercise the legacy
/// `general.hba` branches — including the Unix-socket bypass — without
/// touching the global config.
pub(crate) fn check_hba_with_general(
    general: &General,
    transport: &ClientTransport,
    type_auth: &str,
    username: &str,
    database: &str,
) -> CheckResult {
    if let Some(ref pg) = general.pg_hba {
        return pg.check_hba(transport, type_auth, username, database);
    }
    // Legacy hba list has no unix concept — allow all unix connections
    if transport.is_unix() {
        return CheckResult::Allow;
    }
    if general.hba.is_empty() {
        return CheckResult::Allow;
    }
    let ip = transport.hba_ip();
    if general.hba.iter().any(|net| net.contains(&ip)) {
        CheckResult::Allow
    } else {
        CheckResult::NotMatched
    }
}
