pub mod auth_query;
pub mod hba;
#[cfg(test)]
mod hba_eval_tests;
pub mod jwt;
pub mod pam;
pub mod scram;
pub mod scram_client;
pub mod talos;

use std::marker::Unpin;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::auth::hba::CheckResult;
use log::{error, info, warn};
use once_cell::sync::Lazy;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::auth::jwt::{get_user_name_from_jwt, parse_jwt_pub_key_password};
use crate::auth::pam::pam_auth;
use crate::auth::scram::{
    parse_client_final_message, parse_client_first_message, parse_server_secret,
    prepare_server_final_message, prepare_server_first_response,
};
use crate::config::BackendAuthMethod;
use crate::config::{get_config, PoolMode};
use crate::errors::{ClientIdentifier, Error};
use crate::messages::constants::{
    JWT_PUB_KEY_PASSWORD_PREFIX, MD5_PASSWORD_PREFIX, SASL_CONTINUE, SASL_FINAL, SCRAM_SHA_256,
};
use crate::messages::{
    error_response, error_response_terminal, md5_challenge, md5_hash_password,
    md5_hash_second_pass, plain_password_challenge, read_password, scram_server_response,
    scram_start_challenge, vec_to_string, wrong_password,
};
use crate::pool::{
    create_dynamic_pool, get_auth_query_state, get_pool, get_pool_config, is_dynamic_pool,
    ConnectionPool, PoolIdentifier,
};

const PAM_AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const MAX_CONCURRENT_PAM_AUTH: usize = 16;
static PAM_AUTH_SEMAPHORE: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_PAM_AUTH)));
use crate::server::ServerParameters;
use crate::stats::auth_query::AuthQueryStats;

/// Canonicalised set of GUC names the operator put under
/// `general.startup_parameters` / `pool.startup_parameters` /
/// `auth_query` for the (db, user) pair that authenticated this
/// connection. The same `Arc` lives on `ConnectionPool` so cloning
/// stays zero-copy. Client startup uses this to drop `ParameterStatus`
/// entries the client sent for keys the backend session already has
/// pinned by `startup_parameters`.
pub type OperatorManagedKeys = Arc<std::collections::HashSet<String>>;

/// Outcome of [`authenticate`]: everything the client startup path needs
/// to finish the StartupMessage exchange. `operator_managed_keys` is
/// captured from the same `ConnectionPool` snapshot that produced
/// `server_parameters`, so the client startup filter cannot drift
/// against a concurrent RELOAD or auth_query overlay refetch the way a
/// second global `POOLS` lookup would.
pub struct AuthOutcome {
    pub transaction_mode: bool,
    pub server_parameters: ServerParameters,
    pub prepared_statements_enabled: bool,
    /// Pool user used for backend routing. Usually equal to the authenticated
    /// client username; in auth_query dedicated mode it is auth_query.server_user.
    pub pool_user: String,
    pub operator_managed_keys: Option<OperatorManagedKeys>,
    pub pool_generation: Option<ConnectionPool>,
    pub pool_generation_is_dynamic: bool,
}

/// Authenticate a user based on the provided parameters
pub async fn authenticate<S, T>(
    read: &mut S,
    write: &mut T,
    admin: bool,
    client_identifier: &mut ClientIdentifier,
    pool_name: &str,
    username_from_parameters: &str,
) -> Result<AuthOutcome, Error>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    let mut prepared_statements_enabled = false;

    // Authenticate admin user.
    let (
        transaction_mode,
        server_parameters,
        pool_user,
        operator_managed_keys,
        pool_generation,
        pool_generation_is_dynamic,
    ) = if admin {
        let config = get_config();
        if username_from_parameters != config.general.admin_username {
            let error = Error::AuthError(format!("Invalid admin user: {username_from_parameters}"));
            warn!("{error}");
            wrong_password(write, username_from_parameters).await?;
            return Err(error);
        }

        if client_identifier.hba_md5 == CheckResult::Trust
            || client_identifier.hba_scram == CheckResult::Trust
        {
            info!(
                "HBA trust: admin user={username_from_parameters}, addr={}",
                client_identifier.addr
            );
            return Ok(AuthOutcome {
                transaction_mode: false,
                server_parameters: ServerParameters::admin(),
                prepared_statements_enabled: false,
                pool_user: client_identifier.username.clone(),
                operator_managed_keys: None,
                pool_generation: None,
                pool_generation_is_dynamic: false,
            });
        }
        if client_identifier.hba_md5 == CheckResult::Deny
            || client_identifier.hba_scram == CheckResult::Deny
        {
            let error = Error::AuthError(format!(
                "HBA failed for admin user: {username_from_parameters}"
            ));
            warn!("{error}");
            wrong_password(write, username_from_parameters).await?;
            return Err(error);
        }
        let (tx, sp) = authenticate_admin(
            read,
            write,
            username_from_parameters,
            &config.general.admin_username,
            &config.general.admin_password,
        )
        .await?;
        (
            tx,
            sp,
            client_identifier.username.clone(),
            None,
            None,
            false,
        )
    }
    // Authenticate normal user.
    else {
        authenticate_normal_user(
            read,
            write,
            client_identifier,
            pool_name,
            username_from_parameters,
            &mut prepared_statements_enabled,
        )
        .await?
    };

    Ok(AuthOutcome {
        transaction_mode,
        server_parameters,
        prepared_statements_enabled,
        pool_user,
        operator_managed_keys,
        pool_generation,
        pool_generation_is_dynamic,
    })
}

/// Authenticate an admin user with MD5
async fn authenticate_admin<S, T>(
    read: &mut S,
    write: &mut T,
    username_from_parameters: &str,
    admin_username: &str,
    admin_password: &str,
) -> Result<(bool, ServerParameters), Error>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    // An empty configured admin_password disables the virtual admin console:
    // reject every login attempt outright. Otherwise md5_hash_password would
    // accept a client that simply sends an empty password. The pooler keeps
    // serving normal users; only the admin console is unreachable.
    if admin_password.is_empty() {
        let error = Error::AuthError(format!(
            "admin console disabled: general.admin_password is not set (user {username_from_parameters})"
        ));
        warn!("{error}");
        wrong_password(write, username_from_parameters).await?;
        return Err(error);
    }

    // Authenticate admin user with md5.
    let salt = md5_challenge(write).await?;
    let password_response = read_password(read).await?;

    // Compare server and client hashes.
    let password_hash = md5_hash_password(admin_username, admin_password, &salt);

    // constant-time compare. `Vec::eq` short-circuits on first mismatching
    // byte and leaks the matched prefix length through response timing, enabling
    // a byte-by-byte brute-force of the admin MD5 hash. Especially load-bearing
    // because old generated configs may still carry published admin credentials.
    if !bool::from(password_hash.ct_eq(&password_response)) {
        let error = Error::AuthError(format!(
            "Invalid password for admin user: {username_from_parameters}"
        ));

        warn!("{error}");
        wrong_password(write, username_from_parameters).await?;

        return Err(error);
    }

    Ok((false, ServerParameters::admin()))
}

fn md5_verifier_hash(password: &str) -> Option<&str> {
    let hash = password.strip_prefix(MD5_PASSWORD_PREFIX)?;
    if hash.len() == 32 && hash.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        Some(hash)
    } else {
        None
    }
}

fn md5_verifier_label(password: &str) -> &'static str {
    if password.starts_with(MD5_PASSWORD_PREFIX) {
        "md5"
    } else {
        "scram"
    }
}

async fn unsupported_auth_query_verifier_error<T>(
    write: &mut T,
    stats: &AuthQueryStats,
    username: &str,
    pool_name: &str,
    password_hash: &str,
) -> Result<Error, Error>
where
    T: AsyncWriteExt + Unpin,
{
    stats.auth_failure.fetch_add(1, Ordering::Relaxed);
    error!(
        "[{username}@{pool_name}] auth_query: unsupported password verifier returned by auth_query (len={})",
        password_hash.len()
    );
    error_response_terminal(
        write,
        "Unsupported authentication method for auth_query user.",
        "28P01",
    )
    .await?;
    Ok(Error::AuthError(format!(
        "Unsupported password type for auth_query user: {username}"
    )))
}

async fn auth_query_dynamic_pool_admission_error<T>(
    write: &mut T,
    stats: &AuthQueryStats,
    username: &str,
    pool_name: &str,
    err: Error,
) -> Result<Error, Error>
where
    T: AsyncWriteExt + Unpin,
{
    stats.auth_failure.fetch_add(1, Ordering::Relaxed);
    error!("[{username}@{pool_name}] auth_query: failed to create dynamic pool: {err}");
    error_response(
        write,
        "Unable to create authenticated dynamic pool.",
        "58000",
    )
    .await?;
    Ok(err)
}

async fn reject_scram_passthrough_hba_trust<T>(
    write: &mut T,
    username: &str,
    pool_name: &str,
) -> Result<Error, Error>
where
    T: AsyncWriteExt + Unpin,
{
    error!(
        "[{username}@{pool_name}] HBA trust cannot be used with SCRAM passthrough \
         because backend SCRAM authentication requires a client proof"
    );
    error_response_terminal(
        write,
        "HBA trust cannot be used with SCRAM passthrough authentication.",
        "28000",
    )
    .await?;
    Ok(Error::HbaForbiddenError(format!(
        "HBA trust cannot be used with SCRAM passthrough for {username}@{pool_name}"
    )))
}

/// Authenticate a normal user with various methods
fn eval_hba_for_pool_password(pool_password: &str, ci: &ClientIdentifier) -> CheckResult {
    // Determine HBA outcome based on stored pool password type and HBA checks attached to client identifier
    if ci.is_talos {
        // Already authenticated upstream, allow normal auth flow (not a Trust, but no HBA block)
        return CheckResult::Allow;
    }

    // Empty password is allowed only when HBA is trust for either method
    if pool_password.is_empty()
        && (ci.hba_md5 == CheckResult::Trust || ci.hba_scram == CheckResult::Trust)
    {
        return CheckResult::Trust;
    }

    if pool_password.starts_with(SCRAM_SHA_256) {
        // If SCRAM is trusted or MD5 trust is allowed while SCRAM is not matched, treat as trust
        if ci.hba_scram == CheckResult::Trust
            || (ci.hba_scram == CheckResult::NotMatched && ci.hba_md5 == CheckResult::Trust)
        {
            return CheckResult::Trust;
        }

        // Explicit rejections or no matching rules result in deny
        if ci.hba_scram == CheckResult::Deny
            || (ci.hba_scram == CheckResult::NotMatched
                && (ci.hba_md5 == CheckResult::Deny || ci.hba_md5 == CheckResult::NotMatched))
        {
            return CheckResult::Deny;
        }

        // Otherwise, a password exchange is allowed
        return CheckResult::Allow;
    }

    if pool_password.starts_with(MD5_PASSWORD_PREFIX) && md5_verifier_hash(pool_password).is_none()
    {
        return CheckResult::Deny;
    }

    if md5_verifier_hash(pool_password).is_some() {
        if ci.hba_md5 == CheckResult::Trust {
            return CheckResult::Trust;
        }
        if ci.hba_md5 == CheckResult::NotMatched || ci.hba_md5 == CheckResult::Deny {
            return CheckResult::Deny;
        }
        return CheckResult::Allow;
    }

    // For other auth kinds (JWT/PAM/unknown), the stored verifier shape
    // does not pick a password mechanism, but a matching `reject` HBA
    // line still applies. `reject` sets both hba_md5 and hba_scram to
    // Deny (see HbaLine::check_hba); honor that explicit Deny before the
    // default Allow so a JWT/PAM client cannot bypass a pg_hba reject.
    // Only an explicit Deny blocks here - NotMatched stays Allow so these
    // verifier kinds are not turned into default-deny.
    if ci.hba_md5 == CheckResult::Deny || ci.hba_scram == CheckResult::Deny {
        return CheckResult::Deny;
    }
    CheckResult::Allow
}

fn hba_trust_skips_required_scram_passthrough(
    hba_decision: CheckResult,
    verifier: &str,
    passthrough_requires_client_key: bool,
) -> bool {
    hba_decision == CheckResult::Trust
        && verifier.starts_with(SCRAM_SHA_256)
        && passthrough_requires_client_key
}

fn is_auth_query_shared_pool(pool_id: &PoolIdentifier) -> bool {
    get_auth_query_state(&pool_id.db)
        .and_then(|state| state.shared_pool_id.as_ref().cloned())
        .is_some_and(|shared_pool_id| shared_pool_id == *pool_id)
}

async fn authenticate_normal_user<S, T>(
    read: &mut S,
    write: &mut T,
    client_identifier: &mut ClientIdentifier,
    pool_name: &str,
    username_from_parameters: &str,
    prepared_statements_enabled: &mut bool,
) -> Result<
    (
        bool,
        ServerParameters,
        String,
        Option<OperatorManagedKeys>,
        Option<ConnectionPool>,
        bool,
    ),
    Error,
>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    let mut pool = match get_pool(pool_name, client_identifier.username.as_str()) {
        Some(pool) => {
            // Dynamic pools (created by auth_query passthrough) have empty passwords.
            // Re-authenticate via auth_query to verify credentials on every connection.
            let pool_id = PoolIdentifier::new(pool_name, client_identifier.username.as_str());
            if is_dynamic_pool(&pool_id) || is_auth_query_shared_pool(&pool_id) {
                return try_auth_query(
                    read,
                    write,
                    client_identifier,
                    pool_name,
                    username_from_parameters,
                    prepared_statements_enabled,
                )
                .await;
            }
            pool
        }
        None => {
            // Static user not found — try auth_query
            return try_auth_query(
                read,
                write,
                client_identifier,
                pool_name,
                username_from_parameters,
                prepared_statements_enabled,
            )
            .await;
        }
    };

    let pool_password = pool.settings.user.password.clone();

    // Evaluate HBA once for this connection
    let hba_decision = eval_hba_for_pool_password(&pool_password, client_identifier);
    let backend_auth_snapshot = pool
        .address
        .backend_auth
        .as_ref()
        .map(|ba| ba.read().clone());
    let scram_passthrough_requires_client_key =
        matches!(backend_auth_snapshot, Some(BackendAuthMethod::ScramPending))
            && pool.settings.user.server_password.is_none();
    if hba_decision == CheckResult::Deny {
        error_response_terminal(
        write,
        format!(
            "Connection with scram password from IP address {} to {}@{} is not permitted by HBA configuration. Please contact your database administrator.",
            client_identifier.addr, username_from_parameters, pool_name
        )
            .as_str(),
        "28000",
    )
        .await?;
        return Err(Error::HbaForbiddenError(format!(
        "Connection with scram not permitted by HBA configuration for client: {} from address: {:?}",
        client_identifier, client_identifier.addr,
        )));
    }

    if hba_trust_skips_required_scram_passthrough(
        hba_decision,
        pool_password.as_str(),
        scram_passthrough_requires_client_key,
    ) {
        return Err(
            reject_scram_passthrough_hba_trust(write, username_from_parameters, pool_name).await?,
        );
    }

    if client_identifier.is_talos || hba_decision == CheckResult::Trust {
        // Pass, client already authenticated (talos) or HBA Trust
    } else if pool.settings.user.auth_pam_service.is_some() {
        authenticate_with_pam(
            read,
            write,
            &pool,
            username_from_parameters,
            pool_name,
            &client_identifier.addr,
        )
        .await?;
    } else if pool_password.starts_with(SCRAM_SHA_256) {
        let client_key = authenticate_with_scram(
            read,
            write,
            pool_password.as_str(),
            username_from_parameters,
            pool_name,
            &client_identifier.addr,
            client_identifier.use_tls,
        )
        .await?;

        // For static passthrough: promote ScramPending → ScramPassthrough
        if let Some(ref client_key) = client_key {
            if let Some(ref ba_lock) = pool.address.backend_auth {
                let needs_update = matches!(*ba_lock.read(), BackendAuthMethod::ScramPending);
                if needs_update {
                    *ba_lock.write() = BackendAuthMethod::ScramPassthrough(client_key.clone());
                    info!(
                        "[{username_from_parameters}@{pool_name}] static passthrough: ClientKey stored after SCRAM auth"
                    );
                }
            }
        }
    } else if md5_verifier_hash(&pool_password).is_some() {
        authenticate_with_md5(
            read,
            write,
            pool_password.as_str(),
            username_from_parameters,
            &pool,
            &client_identifier.addr,
        )
        .await?;
    } else if pool_password.starts_with(MD5_PASSWORD_PREFIX) {
        error!(
            "[{username_from_parameters}@{pool_name}] invalid MD5 verifier configured for static user"
        );
        error_response_terminal(
            write,
            "Server authentication configuration error. Please contact your database administrator.",
            "28P01",
        )
        .await?;
        return Err(Error::AuthError(format!(
            "Invalid MD5 verifier configured for static user: {username_from_parameters}"
        )));
    } else if pool_password.starts_with(JWT_PUB_KEY_PASSWORD_PREFIX) {
        let jwt_config = parse_jwt_pub_key_password(&pool_password)?.expect("prefix checked above");
        authenticate_with_jwt(
            read,
            write,
            jwt_config,
            username_from_parameters,
            pool_name,
            &client_identifier.addr,
        )
        .await?;
    } else {
        warn!("[{username_from_parameters}@{pool_name}] unsupported password type");
        error_response_terminal(
            write,
            "Authentication method not supported. Please contact your database administrator.",
            "28P01",
        )
        .await?;
        return Err(Error::AuthError(format!(
            "Unsupported authentication method for user: {username_from_parameters}. Only MD5, SCRAM-SHA-256, JWT, and PAM are supported."
        )));
    }

    let transaction_mode = pool.settings.pool_mode == PoolMode::Transaction;
    *prepared_statements_enabled = transaction_mode && pool.prepared_statement_cache.is_some();

    let server_parameters = match pool.get_server_parameters().await {
        Ok(params) => params,
        Err(err) => {
            // PG-side rejection of an operator-supplied startup
            // parameter already carries the real sqlstate and message
            // from PostgreSQL. Forward them verbatim — same contract
            // the transaction checkout path in
            // src/client/transaction.rs honours — instead of collapsing
            // into the generic 3D000 wrapper.
            if let Error::ServerStartupParameterRejection {
                sqlstate,
                message: pg_message,
                ..
            } = &err
            {
                error!("[{username_from_parameters}@{pool_name}] PG rejected operator-supplied startup parameter: {pg_message}");
                error_response(write, pg_message, sqlstate).await?;
                return Err(err);
            }
            error!("[{username_from_parameters}@{pool_name}] failed to retrieve server parameters: {err}");
            error_response(
                write,
                &format!(
                    "Unable to retrieve server parameters for database: {pool_name}, user: {username_from_parameters}. The database server may be unavailable or misconfigured. Please try again later or contact your database administrator."
                ),
                "3D000",
            )
            .await?;
            return Err(err);
        }
    };

    // Capture operator-managed startup-parameter keys from the same
    // pool snapshot that produced `server_parameters`. The client
    // startup path used to read this set with a second `POOLS` global
    // lookup, which could observe a RELOAD between authentication and
    // the lookup and send `ParameterStatus` values for keys the
    // backend session already has set via operator-managed
    // `startup_parameters`. Snapshotting on this side guarantees the
    // two views stay in step.
    let operator_managed_keys = Some(pool.database.server_pool().operator_managed_startup_keys());

    Ok((
        transaction_mode,
        server_parameters,
        client_identifier.username.clone(),
        operator_managed_keys,
        Some(pool),
        false,
    ))
}

/// Authenticate a user with PAM
async fn authenticate_with_pam<S, T>(
    read: &mut S,
    write: &mut T,
    pool: &ConnectionPool,
    username_from_parameters: &str,
    pool_name: &str,
    client_addr: &str,
) -> Result<(), Error>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    // pam auth.
    plain_password_challenge(write).await?;
    let password_response = read_password(read).await?;
    let password_response = match vec_to_string(password_response) {
        Ok(p) => p,
        Err(err) => {
            error!("[{username_from_parameters}@{pool_name}] PAM: failed to read password from {client_addr}: {err}");
            error_response_terminal(
                write,
                "Invalid password format. Password must be valid UTF-8 text.",
                "28P01",
            )
            .await?;
            return Err(err);
        }
    };
    let service = pool.settings.user.auth_pam_service.clone().unwrap();
    match run_pam_auth_blocking(
        service.clone(),
        username_from_parameters.to_string(),
        password_response,
    )
    .await
    {
        Ok(_) => (),
        Err(err) => {
            error!(
                "[{username_from_parameters}@{pool_name}] PAM authentication failed from {client_addr} (service={service}): {err}"
            );
            error_response_terminal(
                write,
                "Authentication failed. Please check your username and password.",
                "28P01",
            )
            .await?;
            return Err(Error::AuthError(format!(
                "PAM authentication failed for user: {username_from_parameters} with service: {service}"
            )));
        }
    };

    Ok(())
}

async fn run_pam_auth_blocking(
    service: String,
    username: String,
    password: String,
) -> Result<(), Error> {
    let fut = async move {
        let permit = PAM_AUTH_SEMAPHORE
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::AuthError("PAM authentication limiter is closed".to_string()))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            pam_auth(&service, &username, &password)
        })
        .await
        .map_err(|err| Error::AuthError(format!("PAM authentication task failed: {err}")))?
    };

    tokio::time::timeout(PAM_AUTH_TIMEOUT, fut)
        .await
        .map_err(|_| Error::AuthError("PAM authentication timed out".to_string()))?
}

/// Authenticate a user with SCRAM-SHA-256.
/// Returns the ClientKey extracted from the client's SCRAM proof on success.
async fn authenticate_with_scram<S, T>(
    read: &mut S,
    write: &mut T,
    pool_password: &str,
    username_from_parameters: &str,
    pool_name: &str,
    client_addr: &str,
    // RFC 5802 §6 cbind downgrade guard. When TLS is
    // active pg_doorman MUST reject `gs2_flag='y'`; see
    // `parse_client_first_message` for the rationale.
    use_tls: bool,
) -> Result<Option<Vec<u8>>, Error>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    let server_secret = match parse_server_secret(pool_password) {
        Ok(server_secret) => server_secret,
        Err(err) => {
            warn!("[{username_from_parameters}@{pool_name}] SCRAM: failed to parse server secret from {client_addr}: {err}");
            error_response_terminal(
                write,
                "Server authentication configuration error. Please contact your database administrator.",
                "28P01"
            ).await?;
            return Err(Error::ScramServerError(format!(
                "Failed to parse SCRAM server secret for user: {username_from_parameters}"
            )));
        }
    };
    // scram auth.
    scram_start_challenge(write).await?;
    let first_message = read_password(read).await?;
    let client_first_message = match parse_client_first_message(
        String::from_utf8_lossy(&first_message),
        use_tls,
    ) {
        Ok(client_first_message) => client_first_message,
        Err(err) => {
            warn!("[{username_from_parameters}@{pool_name}] SCRAM: client first message parse error from {client_addr}: {err}");
            error_response_terminal(
                    write,
                    "Authentication protocol error. Your client may not support SCRAM authentication properly.",
                    "28P01"
                ).await?;
            return Err(Error::ScramClientError(format!(
                "Failed to parse SCRAM client first message for user: {username_from_parameters}"
            )));
        }
    };
    let server_first_response = prepare_server_first_response(
        client_first_message.nonce.as_str(),
        client_first_message.client_first_bare.as_str(),
        server_secret.salt_base64.as_str(),
        server_secret.iteration,
    );
    scram_server_response(
        write,
        SASL_CONTINUE,
        server_first_response.server_first_bare.as_str(),
    )
    .await?;
    let final_message = read_password(read).await?;
    let client_final_message = match parse_client_final_message(String::from_utf8_lossy(
        &final_message,
    )) {
        Ok(client_final_message) => client_final_message,
        Err(err) => {
            warn!(
                "[{username_from_parameters}@{pool_name}] SCRAM: client final message parse error from {client_addr}: {err}"
            );
            error_response_terminal(
                write,
                "Authentication protocol error. Your client sent an invalid SCRAM final message.",
                "28P01",
            )
            .await?;
            return Err(Error::ScramClientError(format!(
                "Failed to parse SCRAM client final message for user: {username_from_parameters}"
            )));
        }
    };
    let (server_final_message, client_key) = match prepare_server_final_message(
        client_first_message,
        client_final_message,
        server_first_response,
        server_secret.server_key,
        server_secret.stored_key,
    ) {
        Ok(result) => result,
        Err(err) => {
            warn!(
                "[{username_from_parameters}@{pool_name}] SCRAM: server final message error from {client_addr}: {err}"
            );
            error_response_terminal(
                write,
                "Authentication failed. Invalid credentials or authentication protocol error.",
                "28P01",
            )
            .await?;
            return Err(Error::ScramServerError(format!(
                "Failed to prepare SCRAM server final message for user: {username_from_parameters}. This may indicate incorrect password or authentication protocol error."
            )));
        }
    };
    scram_server_response(write, SASL_FINAL, server_final_message.as_str()).await?;

    Ok(Some(client_key))
}

/// Authenticate a user with MD5
async fn authenticate_with_md5<S, T>(
    read: &mut S,
    write: &mut T,
    pool_password: &str,
    username_from_parameters: &str,
    pool: &ConnectionPool,
    client_addr: &str,
) -> Result<(), Error>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    let Some(md5_hash) = md5_verifier_hash(pool_password) else {
        error!(
            "[{username_from_parameters}@{}] invalid MD5 verifier configured for static user",
            pool.address.pool_name
        );
        error_response_terminal(
            write,
            "Server authentication configuration error. Please contact your database administrator.",
            "28P01",
        )
        .await?;
        return Err(Error::AuthError(format!(
            "Invalid MD5 verifier configured for static user: {username_from_parameters}"
        )));
    };
    // md5 auth.
    let salt = md5_challenge(write).await?;
    let password_response = read_password(read).await?;
    let except_md5_hash = md5_hash_second_pass(md5_hash, &salt);
    // constant-time compare prevents timing side-channel byte-by-byte
    // brute force of the user's MD5 hash.
    if !bool::from(except_md5_hash.ct_eq(&password_response)) {
        error!(
            "[{username_from_parameters}@{}] MD5 authentication failed from {client_addr}",
            pool.address.pool_name
        );
        error_response_terminal(
            write,
            "Authentication failed. Please check your username and password.",
            "28P01",
        )
        .await?;
        return Err(Error::AuthError(format!(
            "MD5 authentication failed for user: {username_from_parameters}"
        )));
    }

    Ok(())
}

/// Authenticate a user with JWT
async fn authenticate_with_jwt<S, T>(
    read: &mut S,
    write: &mut T,
    jwt_config: crate::auth::jwt::JwtVerifierConfig,
    username_from_parameters: &str,
    pool_name: &str,
    client_addr: &str,
) -> Result<(), Error>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    // jwt.
    plain_password_challenge(write).await?;
    let jwt_token_response = read_password(read).await?;
    let jwt_token = match vec_to_string(jwt_token_response) {
        Ok(p) => p,
        Err(err) => {
            error!("[{username_from_parameters}@{pool_name}] JWT: failed to parse token from {client_addr}: {err}");
            error_response_terminal(
                write,
                "Invalid JWT token format. Token must be valid UTF-8 text.",
                "28P01",
            )
            .await?;
            return Err(Error::JWTValidate(format!(
                "Failed to parse JWT token as UTF-8 for user: {username_from_parameters}"
            )));
        }
    };
    let jwt_user_name = match get_user_name_from_jwt(
        jwt_config.key_filename,
        jwt_token,
        &jwt_config.issuer,
        &jwt_config.audience,
    )
    .await
    {
        Ok(u) => u,
        Err(err) => {
            error!("[{username_from_parameters}@{pool_name}] JWT: validation failed from {client_addr}: {err}");
            error_response_terminal(
                write,
                "JWT token validation failed. Please provide a valid token.",
                "28P01",
            )
            .await?;
            return Err(Error::JWTValidate(format!(
                "JWT token validation failed for user: {username_from_parameters}. Token may be expired, malformed, or signed with wrong key."
            )));
        }
    };
    if !jwt_user_name.eq(username_from_parameters) {
        error!("[{username_from_parameters}@{pool_name}] JWT: username mismatch from {client_addr} (token={jwt_user_name})");
        error_response_terminal(
            write,
            format!("JWT token username mismatch. Token contains username '{jwt_user_name}' but you're trying to connect as '{username_from_parameters}'.").as_str(),
            "28P01"
        ).await?;
        return Err(Error::JWTValidate(format!(
            "JWT token username mismatch: token contains '{jwt_user_name}' but connection requested for '{username_from_parameters}'"
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Auth query authentication (MD5, server_user mode)
// ---------------------------------------------------------------------------

/// Authenticate a user via auth_query: fetch password hash from cache/PG,
/// run MD5 challenge-response, then return the shared pool + server params.
///
/// On success, keeps `client_identifier.username` as the authenticated client
/// user and returns the backend pool user separately for routing.
async fn try_auth_query<S, T>(
    read: &mut S,
    write: &mut T,
    client_identifier: &mut ClientIdentifier,
    pool_name: &str,
    username: &str,
    prepared_statements_enabled: &mut bool,
) -> Result<
    (
        bool,
        ServerParameters,
        String,
        Option<OperatorManagedKeys>,
        Option<ConnectionPool>,
        bool,
    ),
    Error,
>
where
    S: AsyncReadExt + Unpin,
    T: AsyncWriteExt + Unpin,
{
    // Helper: record auth failure stat
    macro_rules! auth_fail {
        ($state:expr) => {
            $state.stats.auth_failure.fetch_add(1, Ordering::Relaxed);
        };
    }

    // 1. Check if auth_query is configured for this pool
    let aq_state = match get_auth_query_state(pool_name) {
        Some(state) => state,
        None => {
            // Differentiate: auth_query configured but executor not ready vs not configured
            let msg = if get_pool_config(pool_name).is_some_and(|c| c.auth_query.is_some()) {
                format!(
                    "Auth query service temporarily unavailable for database: {pool_name}. \
                     Please try again later."
                )
            } else {
                format!(
                    "No connection pool configured for database: {pool_name}, \
                     user: {username}. Please check your connection parameters."
                )
            };
            error_response(write, &msg, "3D000").await?;
            return Err(Error::AuthError(msg));
        }
    };

    // 2. Get cache (lazily initializes executor on first call)
    let cache = match aq_state.cache().await {
        Ok(cache) => cache,
        Err(err) => {
            error!("[{username}@{pool_name}] auth_query: executor initialization failed: {err}");
            error_response(
                write,
                "Authentication service unavailable. Please try again later.",
                "58000",
            )
            .await?;
            return Err(err);
        }
    };

    // 3. Fetch password hash from cache or PG. `mut` because a
    //    successful MD5 refetch below swaps in the fresh entry so the
    //    backend pool gets the rotated password hash and the rotated
    //    per-user startup_parameters, not the stale snapshot.
    let mut cache_entry = match cache.get_or_fetch(username).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            // User not found
            auth_fail!(aq_state);
            warn!("[{username}@{pool_name}] auth_query: user not found");
            wrong_password(write, username).await?;
            return Err(Error::AuthError(format!(
                "auth_query: user '{username}' not found in pool '{pool_name}'"
            )));
        }
        Err(err) => {
            error!("[{username}@{pool_name}] auth_query: failed to fetch password: {err}");
            error_response(
                write,
                "Authentication service unavailable. Please try again later.",
                "58000",
            )
            .await?;
            return Err(err);
        }
    };

    // 4. HBA check
    let hba_decision = eval_hba_for_pool_password(&cache_entry.password_hash, client_identifier);
    if hba_decision == CheckResult::Deny {
        error_response_terminal(
            write,
            &format!(
                "Connection from IP address {} to {}@{} is not permitted by HBA configuration.",
                client_identifier.addr, username, pool_name
            ),
            "28000",
        )
        .await?;
        return Err(Error::HbaForbiddenError(format!(
            "HBA denied auth_query user '{username}' from {:?}",
            client_identifier.addr,
        )));
    }

    // 5. Authenticate based on password type
    let mut auth_client_key: Option<Vec<u8>> = None;
    if hba_trust_skips_required_scram_passthrough(
        hba_decision,
        &cache_entry.password_hash,
        aq_state.shared_pool_id.is_none(),
    ) {
        auth_fail!(aq_state);
        return Err(reject_scram_passthrough_hba_trust(write, username, pool_name).await?);
    }

    if hba_decision == CheckResult::Trust {
        // HBA trust — skip password check
    } else if let Some(md5_hash) = md5_verifier_hash(&cache_entry.password_hash) {
        // MD5 challenge-response
        let salt = md5_challenge(write).await?;
        let password_response = read_password(read).await?;
        let expected = md5_hash_second_pass(md5_hash, &salt);

        // constant-time compare on auth_query-cached MD5 hash.
        if !bool::from(expected.ct_eq(&password_response)) {
            // Password mismatch - try re-fetch (password may have changed in PG)
            let mut auth_ok = false;
            let mut refreshed: Option<std::sync::Arc<crate::auth::auth_query::CacheEntry>> = None;
            if let Ok(Some(new_entry)) = cache.refetch_on_failure(username).await {
                if new_entry.password_hash != cache_entry.password_hash {
                    if let Some(new_md5_hash) = md5_verifier_hash(&new_entry.password_hash) {
                        let new_expected = md5_hash_second_pass(new_md5_hash, &salt);
                        if bool::from(new_expected.ct_eq(&password_response)) {
                            auth_ok = true;
                            info!(
                                "[{username}@{pool_name}] auth_query: re-fetched password matched"
                            );
                            refreshed = Some(new_entry);
                        }
                    } else {
                        // The refetched verifier is no longer MD5 — the
                        // operator switched `password_encryption` mid-flight
                        // (typically MD5 → SCRAM). The current MD5 proof
                        // cannot validate against a SCRAM verifier; reject
                        // this attempt and invalidate the cache so the next
                        // reconnect hits `cache.get_or_fetch` and takes the
                        // SCRAM branch immediately rather than waiting for
                        // `cache_ttl`.
                        warn!(
                            "[{username}@{pool_name}] auth_query: refetched verifier changed type ({stored} → {fresh}); cache invalidated, client must reconnect with the new mechanism",
                            stored = md5_verifier_label(&cache_entry.password_hash),
                            fresh = md5_verifier_label(&new_entry.password_hash)
                        );
                        cache.invalidate(username);
                    }
                }
            }
            if !auth_ok {
                auth_fail!(aq_state);
                warn!(
                    "[{username}@{pool_name}] auth_query: MD5 authentication failed (refetch did not match or was rate-limited)"
                );
                wrong_password(write, username).await?;
                return Err(Error::AuthError(format!(
                    "MD5 authentication failed for auth_query user: {username}"
                )));
            }
            // Swap in the refetched snapshot so backend_auth and the
            // dynamic-pool overlay below are built from the rotated
            // credentials, not the stale ones that just failed the
            // first challenge.
            if let Some(new_entry) = refreshed {
                cache_entry = new_entry;
            }
        }
    } else if cache_entry.password_hash.starts_with(MD5_PASSWORD_PREFIX) {
        auth_fail!(aq_state);
        error!("[{username}@{pool_name}] auth_query: invalid MD5 verifier returned by auth_query");
        error_response_terminal(
            write,
            "Server authentication configuration error. Please contact your database administrator.",
            "28P01",
        )
        .await?;
        return Err(Error::AuthError(format!(
            "Invalid MD5 verifier for auth_query user: {username}"
        )));
    } else if cache_entry.password_hash.starts_with(SCRAM_SHA_256) {
        // SCRAM-SHA-256 challenge-response
        let server_secret = match parse_server_secret(&cache_entry.password_hash) {
            Ok(s) => s,
            Err(err) => {
                error!(
                    "[{username}@{pool_name}] auth_query: failed to parse SCRAM verifier: {err}"
                );
                error_response_terminal(
                    write,
                    "Server authentication configuration error. Please contact your database administrator.",
                    "28P01",
                )
                .await?;
                return Err(Error::ScramServerError(format!(
                    "Failed to parse SCRAM server secret for auth_query user: {username}"
                )));
            }
        };

        scram_start_challenge(write).await?;
        let first_msg = read_password(read).await?;
        let client_first = match parse_client_first_message(
            String::from_utf8_lossy(&first_msg),
            client_identifier.use_tls,
        ) {
            Ok(msg) => msg,
            Err(err) => {
                warn!("[{username}@{pool_name}] auth_query: SCRAM client first message parse error: {err}");
                error_response_terminal(
                    write,
                    "Authentication protocol error. Your client may not support SCRAM authentication properly.",
                    "28P01",
                )
                .await?;
                return Err(Error::ScramClientError(format!(
                    "Failed to parse SCRAM client first message for auth_query user: {username}"
                )));
            }
        };

        let server_first = prepare_server_first_response(
            &client_first.nonce,
            &client_first.client_first_bare,
            &server_secret.salt_base64,
            server_secret.iteration,
        );
        scram_server_response(write, SASL_CONTINUE, &server_first.server_first_bare).await?;

        let final_msg = read_password(read).await?;
        let client_final = match parse_client_final_message(String::from_utf8_lossy(&final_msg)) {
            Ok(msg) => msg,
            Err(err) => {
                warn!("[{username}@{pool_name}] auth_query: SCRAM client final message parse error: {err}");
                error_response_terminal(
                    write,
                    "Authentication protocol error. Your client sent an invalid SCRAM final message.",
                    "28P01",
                )
                .await?;
                return Err(Error::ScramClientError(format!(
                    "Failed to parse SCRAM client final message for auth_query user: {username}"
                )));
            }
        };

        match prepare_server_final_message(
            client_first,
            client_final,
            server_first,
            server_secret.server_key,
            server_secret.stored_key,
        ) {
            Ok((server_final, client_key)) => {
                scram_server_response(write, SASL_FINAL, &server_final).await?;
                // Store ClientKey in cache for future SCRAM passthrough
                cache.set_client_key(username, client_key.clone());
                auth_client_key = Some(client_key);
            }
            Err(_) => {
                // SCRAM auth failed — password may have rotated (new salt).
                // Unlike MD5, SCRAM proof is bound to the salt from the verifier,
                // so we can't retry with a re-fetched verifier using the same proof.
                // Invalidate cache so next reconnect gets fresh verifier.
                auth_fail!(aq_state);
                cache.invalidate(username);
                error!(
                    "[{username}@{pool_name}] auth_query: SCRAM authentication failed, cache invalidated"
                );
                wrong_password(write, username).await?;
                return Err(Error::AuthError(format!(
                    "SCRAM authentication failed for auth_query user: {username}. Cache invalidated — please reconnect."
                )));
            }
        }
    } else {
        return Err(unsupported_auth_query_verifier_error(
            write,
            &aq_state.stats,
            username,
            pool_name,
            &cache_entry.password_hash,
        )
        .await?);
    }

    // 6. Route to shared pool (dedicated) or dynamic pool (passthrough)
    match aq_state.shared_pool_id {
        Some(ref shared_pool_id) => {
            // === Dedicated mode: all dynamic users share the server_user pool ===
            let pool_user = shared_pool_id.user.clone();

            let mut pool = match get_pool(&shared_pool_id.db, &shared_pool_id.user) {
                Some(pool) => pool,
                None => {
                    error!(
                        "[{username}@{pool_name}] auth_query: shared pool {}@{} not found",
                        shared_pool_id.user, shared_pool_id.db
                    );
                    error_response(write, "Internal pool configuration error.", "58000").await?;
                    return Err(Error::AuthError(format!(
                        "auth_query shared pool not found: {shared_pool_id}"
                    )));
                }
            };

            let transaction_mode = pool.settings.pool_mode == PoolMode::Transaction;
            *prepared_statements_enabled =
                transaction_mode && pool.prepared_statement_cache.is_some();

            let server_parameters = match pool.get_server_parameters().await {
                Ok(params) => params,
                Err(err) => {
                    // Forward PG-rejected operator startup parameter
                    // verbatim, same as the static-user path above.
                    if let Error::ServerStartupParameterRejection {
                        sqlstate,
                        message: pg_message,
                        ..
                    } = &err
                    {
                        error!("[{username}@{pool_name}] auth_query: PG rejected operator-supplied startup parameter: {pg_message}");
                        error_response(write, pg_message, sqlstate).await?;
                        return Err(err);
                    }
                    error!(
                        "[{username}@{pool_name}] auth_query: failed to get server parameters: {err}"
                    );
                    error_response(
                        write,
                        "Unable to retrieve server parameters. Please try again later.",
                        "58000",
                    )
                    .await?;
                    return Err(err);
                }
            };

            aq_state.stats.auth_success.fetch_add(1, Ordering::Relaxed);
            info!(
                "[{username}@{pool_name}] auth_query: authenticated, using shared pool '{shared_pool_id}'"
            );

            let operator_managed_keys =
                Some(pool.database.server_pool().operator_managed_startup_keys());
            Ok((
                transaction_mode,
                server_parameters,
                pool_user,
                operator_managed_keys,
                Some(pool),
                false,
            ))
        }
        None => {
            // === Passthrough mode: each dynamic user gets their own pool ===
            // After an MD5 refetch matched the rotated password,
            // `cache_entry` already points at the new snapshot, so
            // `password_hash` and `startup_parameters` below reflect the
            // credentials PG will accept on the backend side.
            let backend_auth = if md5_verifier_hash(&cache_entry.password_hash).is_some() {
                Some(BackendAuthMethod::Md5PassTheHash(
                    cache_entry.password_hash.clone(),
                ))
            } else {
                auth_client_key.map(BackendAuthMethod::ScramPassthrough)
            };

            // Use the overlay from the same auth_query row that
            // authenticated this user. That keeps dynamic-pool creation
            // tied to this login instead of reading the global cache
            // again while TTL expiry or a concurrent refetch is changing it.
            let fetched_overlay = Arc::clone(cache_entry.startup_overlay.map());
            let fetched_overlay_hash = cache_entry.startup_overlay.hash();
            let (mut pool, init_guard) = match create_dynamic_pool(
                pool_name,
                username,
                &aq_state,
                backend_auth,
                fetched_overlay,
                fetched_overlay_hash,
            ) {
                Ok(created) => created,
                Err(err) => {
                    return Err(auth_query_dynamic_pool_admission_error(
                        write,
                        &aq_state.stats,
                        username,
                        pool_name,
                        err,
                    )
                    .await?);
                }
            };

            // Do NOT change client_identifier.username — stay as the dynamic user
            // so that Client.username matches the pool's user for get_pool() lookups.

            let transaction_mode = pool.settings.pool_mode == PoolMode::Transaction;
            *prepared_statements_enabled =
                transaction_mode && pool.prepared_statement_cache.is_some();

            let server_parameters = match pool.get_server_parameters().await {
                Ok(params) => params,
                Err(err) => {
                    if let Error::ServerStartupParameterRejection {
                        sqlstate,
                        message: pg_message,
                        ..
                    } = &err
                    {
                        error!("[{username}@{pool_name}] auth_query passthrough: PG rejected operator-supplied startup parameter: {pg_message}");
                        // Invalidate the cache so concurrent reconnects
                        // see the new (or, if user fixes the row, fixed)
                        // entry instead of the still-cached bad overlay.
                        // The pool entry itself is removed by
                        // `init_guard` falling out of scope without a
                        // `commit`.
                        cache.invalidate(username);
                        error_response(write, pg_message, sqlstate).await?;
                        return Err(err);
                    }
                    error!("[{username}@{pool_name}] auth_query: passthrough pool failed: {err}");
                    error_response(
                        write,
                        "Unable to connect to database server. Please try again later.",
                        "58000",
                    )
                    .await?;
                    return Err(err);
                }
            };

            // First connection established — release the guard so GC
            // resumes normal behavior for this pool.
            init_guard.commit();

            aq_state.stats.auth_success.fetch_add(1, Ordering::Relaxed);
            info!("[{username}@{pool_name}] auth_query: authenticated (passthrough mode)");

            let operator_managed_keys =
                Some(pool.database.server_pool().operator_managed_startup_keys());
            Ok((
                transaction_mode,
                server_parameters,
                username.to_string(),
                operator_managed_keys,
                Some(pool),
                true,
            ))
        }
    }
}

#[cfg(test)]
mod mocks;
#[cfg(test)]
mod tests;
