//! Admin interface for PgDoorman.
//!
//! This module provides administrative commands for managing the connection pooler,
//! including SHOW commands for statistics and RELOAD/SHUTDOWN commands.

mod commands;
mod show;

pub mod events;
pub mod operations;

use bytes::{Buf, BufMut, BytesMut};
use log::{debug, warn};

use crate::app::log_level;
use crate::errors::Error;
use crate::messages::protocol::{command_complete, data_row, row_description};
use crate::messages::types::DataType;
use crate::messages::write_all_half;
use crate::pool::ClientServerMap;

/// Canonical list of SHOW subcommands. Single source of truth for:
/// - SHOW dispatch (match arms below)
/// - SHOW HELP output (show.rs)
/// - psql tab-completion (handle_tab_completion)
pub(crate) const SHOW_SUBCOMMANDS: &[&str] = &[
    "help",
    "config",
    "databases",
    "pools",
    "pools_extended",
    "pools_memory",
    "pool_coordinator",
    "pool_scaling",
    "prepared_statements",
    "interner",
    "clients",
    "servers",
    "connections",
    "stats",
    "version",
    "users",
    "auth_query",
    "startup_parameters",
    "log_level",
    "lists",
    #[cfg(target_os = "linux")]
    "sockets",
];

#[cfg(not(windows))]
use commands::upgrade;
use commands::{pause, reconnect, reload, resume, shutdown};
#[cfg(target_os = "linux")]
use show::show_sockets;
use show::{
    reset_interner, show_auth_query, show_clients, show_config, show_connections, show_databases,
    show_help, show_interner, show_interner_top, show_lists, show_log_level, show_pool_coordinator,
    show_pool_scaling, show_pools, show_pools_extended, show_pools_memory,
    show_prepared_statements, show_servers, show_startup_parameters, show_stats, show_users,
    show_version,
};

/// Handle admin client.
pub async fn handle_admin<T>(
    stream: &mut T,
    mut query: BytesMut,
    client_server_map: ClientServerMap,
) -> Result<(), Error>
where
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    let code = query.get_u8() as char;

    if code != 'Q' {
        // legacy code returned `Err(ProtocolSyncError)`,
        // which propagated up to `client.handle()` -> `process_error` is
        // skipped for admin and the socket was dropped without an
        // ErrorResponse. Drivers that default to the extended-query protocol
        // (psycopg3, asyncpg, pgjdbc with simpleProtocolOnly=false, npgsql)
        // then surfaced `OperationalError: server closed the connection
        // unexpectedly` on `SHOW POOLS` etc. Reply with an honest
        // `feature_not_supported` ErrorResponse + ReadyForQuery so the
        // client can keep its socket alive and reissue via simple-query
        // (psycopg3 `ClientCursor`, asyncpg `conn.execute`, pgjdbc
        // `prepareThreshold=0` or `simpleProtocolOnly=true`). Same shape as
        // the cancel / GSS / DEALLOCATE / FunctionCall change fixes: a
        // legitimate client request is now refused with a protocol-correct
        // signal instead of a silent drop.
        return write_admin_error(
            stream,
            &format!(
                "extended query protocol not supported on admin database (got message code '{code}'); use simple query"
            ),
            "0A000",
        )
        .await;
    }

    // Reject invalid admin query frame lengths before converting to
    // `usize`; frames shorter than header plus trailing NUL cannot be
    // parsed safely.
    let len_raw = query.get_i32();
    if len_raw < 5 {
        return Err(Error::ProtocolSyncError(format!(
            "Admin Q frame length {len_raw} below 5-byte minimum (header + NUL)"
        )));
    }
    let body_len = (len_raw as usize).saturating_sub(5);
    let remaining = query.remaining();
    if body_len > remaining {
        return Err(Error::ProtocolSyncError(format!(
            "Admin Q frame declared body {body_len} bytes but only {remaining} bytes remain"
        )));
    }
    let query = String::from_utf8_lossy(&query[..body_len]).to_string();

    debug!("Admin query: {query}");

    // Intercept psql tab-completion queries to pg_catalog.pg_settings
    if query.contains("pg_catalog.pg_settings") {
        return handle_tab_completion(stream, &query).await;
    }

    // split on `;` FIRST so semicolons attached to
    // intermediate tokens don't poison the dispatch match. Previously
    // `query.trim_end_matches(';').split_whitespace()` produced
    // `["SHOW", "POOLS;", "SHOW", "DATABASES"]` for `SHOW POOLS; SHOW
    // DATABASES`, and `query_parts[1] == "POOLS;"` failed the SHOW
    // dispatch (no match arm with the trailing `;`) - so even the FIRST
    // statement returned `"Unsupported SHOW query"`. Take the first
    // non-empty statement and tokenize it cleanly; subsequent
    // statements in the same Q frame are dropped (admin runs one
    // command per frame, mirroring pgbouncer; see pgbouncer docs
    // `https://www.pgbouncer.org/usage.html`).
    let first_stmt = query
        .split(';')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let query_parts: Vec<&str> = first_stmt.split_whitespace().collect();

    // Empty admin Q frames, e.g. body just `;\0` or whitespace, have no first
    // token after splitting. Reject them before indexing into `query_parts`.
    let first_token = match query_parts.first() {
        Some(t) => t,
        None => {
            return Err(Error::ProtocolSyncError(
                "empty admin query (no tokens after whitespace/`;` strip)".to_string(),
            ));
        }
    };

    match first_token.to_ascii_uppercase().as_str() {
        "SET" => set_command(stream, &query_parts).await,
        "RELOAD" => reload(stream, client_server_map).await,
        "SHUTDOWN" => shutdown(stream).await,
        #[cfg(not(windows))]
        "UPGRADE" => upgrade(stream).await,
        "PAUSE" => {
            let db = query_parts.get(1).map(|s| s.to_string());
            pause(stream, db).await
        }
        "RESUME" => {
            let db = query_parts.get(1).map(|s| s.to_string());
            resume(stream, db).await
        }
        "RECONNECT" => {
            let db = query_parts.get(1).map(|s| s.to_string());
            reconnect(stream, db).await
        }
        "SHOW" => {
            if query_parts.len() < 2 {
                warn!("unsupported admin subcommand for SHOW: {query_parts:?}");
                write_admin_error(
                    stream,
                    "Unsupported query against the admin database, please use SHOW HELP for a list of supported subcommands",
                    "58000",
                )
                .await
            } else {
                match query_parts[1].to_ascii_uppercase().as_str() {
                    "HELP" => show_help(stream).await,
                    "CONFIG" => show_config(stream).await,
                    "DATABASES" => show_databases(stream).await,
                    "LISTS" => show_lists(stream).await,
                    "POOLS" => show_pools(stream).await,
                    "POOLS_EXTENDED" => show_pools_extended(stream).await,
                    "POOLS_MEMORY" | "POOL_MEMORY" => show_pools_memory(stream).await,
                    "PREPARED_STATEMENTS" => show_prepared_statements(stream).await,
                    "INTERNER" => match query_parts.get(2).and_then(|s| s.parse::<usize>().ok()) {
                        Some(n) => {
                            show_interner_top(stream, normalize_show_interner_top_n(n)).await
                        }
                        None => show_interner(stream).await,
                    },
                    "CLIENTS" => show_clients(stream).await,
                    "SERVERS" => show_servers(stream).await,
                    "CONNECTIONS" => show_connections(stream).await,
                    "STATS" => show_stats(stream).await,
                    "VERSION" => show_version(stream).await,
                    "USERS" => show_users(stream).await,
                    "AUTH_QUERY" => show_auth_query(stream).await,
                    "STARTUP_PARAMETERS" => show_startup_parameters(stream).await,
                    "POOL_COORDINATOR" => show_pool_coordinator(stream).await,
                    "POOL_SCALING" => show_pool_scaling(stream).await,
                    "LOG_LEVEL" => show_log_level(stream).await,
                    #[cfg(target_os = "linux")]
                    "SOCKETS" => show_sockets(stream).await,
                    _ => {
                        warn!(
                            "unsupported admin subcommand for SHOW: {}",
                            query_parts[1].to_ascii_uppercase().as_str()
                        );
                        write_admin_error(
                            stream,
                            "Unsupported SHOW query against the admin database",
                            "58000",
                        )
                        .await
                    }
                }
            }
        }
        "RESET" => {
            if query_parts.len() == 2 && query_parts[1].eq_ignore_ascii_case("INTERNER") {
                reset_interner(stream).await
            } else {
                warn!("unsupported admin RESET target: {query_parts:?}");
                write_admin_error(
                    stream,
                    "Unsupported RESET target — only RESET INTERNER is supported",
                    "58000",
                )
                .await
            }
        }
        _ => {
            warn!(
                "unsupported admin command: {}",
                query_parts[0].to_ascii_uppercase().as_str()
            );
            write_admin_error(
                stream,
                "Unsupported query against the admin database",
                "58000",
            )
            .await
        }
    }
}

fn normalize_show_interner_top_n(n: usize) -> usize {
    let requested = u64::try_from(n).unwrap_or(u64::MAX);
    crate::web::routes::collect::clamp_top_n(requested) as usize
}

/// Respond to psql tab-completion queries that reference pg_catalog.pg_settings.
/// psql sends these automatically when the user presses TAB after SET or SHOW.
async fn handle_tab_completion<T>(stream: &mut T, query: &str) -> Result<(), Error>
where
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    let query_lower = query.to_ascii_lowercase();
    let mut res = BytesMut::new();

    if query_lower.contains("unnest(enumvals)") {
        // SET log_level = <TAB> — return enum values for the parameter
        res.put(row_description(&vec![("val", DataType::Text)]));
        for val in &["error", "warn", "info", "debug", "trace", "off", "default"] {
            res.put(data_row(&[val.to_string()]));
        }
    } else if query_lower.contains("vartype") {
        // Type lookup — psql checks if parameter is enum/bool/string
        res.put(row_description(&vec![("vartype", DataType::Text)]));
        res.put(data_row(&["enum".to_string()]));
    } else if query_lower.contains("context") {
        // SET <TAB> — return settable parameters (filtered by context)
        res.put(row_description(&vec![("name", DataType::Text)]));
        res.put(data_row(&["log_level".to_string()]));
    } else {
        // SHOW <TAB> — return all SHOW subcommands from the canonical list
        res.put(row_description(&vec![("name", DataType::Text)]));
        for name in SHOW_SUBCOMMANDS {
            res.put(data_row(&[name.to_string()]));
        }
    }

    res.put(command_complete("SELECT"));
    res.put_u8(b'Z');
    res.put_i32(5);
    res.put_u8(b'I');
    write_all_half(stream, &res).await
}

/// Send an ERROR-severity response through the bounded admin writer.
async fn write_admin_error<T>(stream: &mut T, message: &str, code: &str) -> Result<(), Error>
where
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    let mut error = BytesMut::new();
    error.put_u8(b'S');
    error.put_slice(b"ERROR\0");
    error.put_u8(b'V');
    error.put_slice(b"ERROR\0");
    error.put_u8(b'C');
    error.put_slice(format!("{code}\0").as_bytes());
    error.put_u8(b'M');
    error.put_slice(format!("{message}\0").as_bytes());
    error.put_u8(0);

    let mut res = BytesMut::new();
    res.put_u8(b'E');
    res.put_i32(error.len() as i32 + 4);
    res.put(error);
    res.put_u8(b'Z');
    res.put_i32(5);
    res.put_u8(b'I');
    write_all_half(stream, &res).await
}

/// Handle SET command. Currently supports: SET log_level = '<filter>'
async fn set_command<T>(stream: &mut T, query_parts: &[&str]) -> Result<(), Error>
where
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    // Parse: SET log_level = 'value' or SET log_level 'value' or SET log_level value
    if query_parts.len() < 3 {
        return write_admin_error(stream, "SET requires: SET <parameter> = '<value>'", "42601")
            .await;
    }

    let param = query_parts[1].to_ascii_uppercase();
    // Collect value: skip "=" if present, join remaining parts
    let value_parts: Vec<&str> = query_parts[2..]
        .iter()
        .filter(|s| **s != "=")
        .copied()
        .collect();
    let value = value_parts.join(" ");
    let value = value.trim().trim_matches('\'').trim_matches('"');

    match param.as_str() {
        "LOG_LEVEL" => match log_level::set_log_level(value) {
            Ok(()) => {
                log::info!("SET log_level = '{}'", log_level::get_log_level());
                // Re-export pg_doorman_log_level so the live filter
                // shows up in /metrics on the next scrape — operators
                // can confirm a mid-incident `SET log_level = debug`
                // landed without grepping logs.
                crate::web::metrics::refresh_static_info_metrics();
                let mut res = BytesMut::new();
                res.put(command_complete("SET"));
                res.put_u8(b'Z');
                res.put_i32(5);
                res.put_u8(b'I');
                write_all_half(stream, &res).await
            }
            Err(err) => write_admin_error(stream, &err, "42601").await,
        },
        _ => {
            write_admin_error(
                stream,
                &format!("Unknown SET parameter: {param}. Supported: log_level"),
                "42601",
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_subcommands_contains_startup_parameters() {
        // Tab completion on `SHOW <TAB>` returns SHOW_SUBCOMMANDS, and the
        // dispatch above routes `SHOW STARTUP_PARAMETERS` to
        // `show_startup_parameters`. The entry has to exist in this constant
        // for psql's autocomplete to surface the command, since the
        // canonical list is the single source of truth shared between
        // dispatch, `SHOW HELP`, and `handle_tab_completion`.
        assert!(
            SHOW_SUBCOMMANDS.contains(&"startup_parameters"),
            "SHOW_SUBCOMMANDS missing startup_parameters: {SHOW_SUBCOMMANDS:?}"
        );
    }

    #[test]
    fn show_interner_top_n_is_bounded() {
        assert_eq!(normalize_show_interner_top_n(0), 20);
        assert_eq!(normalize_show_interner_top_n(1), 1);
        assert_eq!(normalize_show_interner_top_n(200), 200);
        assert_eq!(normalize_show_interner_top_n(usize::MAX), 200);
    }

    /// `psql -c "SHOW POOLS; SHOW DATABASES"` ships
    /// both statements in one `'Q'` frame. The legacy
    /// `query.trim_end_matches(';').split_whitespace()` left `'POOLS;'`
    /// as a single token, so `query_parts[1]` had a trailing `';'` and
    /// no SHOW match arm matched it - even the FIRST statement returned
    /// `"Unsupported SHOW query"`. The fix splits on `;` first; this
    /// test pins the tokenizer behaviour so the regression cannot creep
    /// back unnoticed.
    #[test]
    fn admin_tokenizer_splits_on_semicolon_first() {
        // Inline the same tokenizer the production code uses so the
        // tokenizer logic is the unit under test, without spinning up
        // a full BDD scenario.
        let tokenize = |q: &str| -> Vec<String> {
            let first_stmt = q
                .split(';')
                .map(str::trim)
                .find(|s| !s.is_empty())
                .unwrap_or("");
            first_stmt.split_whitespace().map(str::to_string).collect()
        };

        assert_eq!(tokenize("SHOW POOLS;"), vec!["SHOW", "POOLS"]);
        assert_eq!(tokenize("SHOW POOLS"), vec!["SHOW", "POOLS"]);
        // Multi-statement Q from psql `-c "...; ..."` - first stmt is
        // taken cleanly; the second is intentionally dropped.
        assert_eq!(
            tokenize("SHOW POOLS; SHOW DATABASES"),
            vec!["SHOW", "POOLS"]
        );
        // Leading garbage / empty statements are skipped.
        assert_eq!(tokenize("  ;  ; SHOW POOLS;"), vec!["SHOW", "POOLS"]);
        // All-empty input falls through to an empty Vec (caller emits
        // the typed "empty admin query" error).
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize(";;;"), Vec::<String>::new());
    }

    #[test]
    fn admin_sql_errors_use_bounded_admin_writer() {
        let src = include_str!("mod.rs");
        let production_src = src
            .split("#[cfg(test)]")
            .next()
            .expect("admin module must have production section before tests");
        let import_start = src
            .find("use crate::messages::protocol::")
            .expect("protocol import must exist");
        let import_end = src[import_start..]
            .find(';')
            .map(|offset| import_start + offset)
            .expect("protocol import must end with semicolon");
        let protocol_import = &src[import_start..=import_end];

        assert!(
            !protocol_import.contains("error_response"),
            "admin SQL errors must not import the unbounded protocol error_response writer"
        );
        assert!(
            !production_src.contains("error_response(stream"),
            "admin SQL errors must use write_admin_error/write_all_half"
        );
    }
}
