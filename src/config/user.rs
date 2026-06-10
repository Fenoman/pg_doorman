//! PostgreSQL user configuration.

use serde_derive::{Deserialize, Serialize};

use crate::auth::jwt::{parse_jwt_pub_key_password, validate_jwt_pub_key_file};
use crate::errors::Error;
use crate::messages::JWT_PUB_KEY_PASSWORD_PREFIX;

use super::pool::validate_prewarm_query_does_not_set_session_state;
use super::{PoolMode, MAX_POOL_SIZE};

/// PostgreSQL user.
#[derive(Clone, PartialEq, Hash, Eq, Serialize, Deserialize, Debug)]
pub struct User {
    pub username: String,
    pub password: String,
    pub pool_size: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_pool_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_mode: Option<PoolMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_lifetime: Option<u64>,
    // Override backend credentials. When omitted, passthrough auth is used:
    // pg_doorman reuses the client's MD5 hash or SCRAM ClientKey to authenticate.
    // Only needed when the backend PostgreSQL user differs from the pool username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_password: Option<String>,
    /// Per-user override of the pool-level `prewarm_query`. When `Some`, it
    /// replaces the pool's value entirely (even when set to `Some(String::new())`
    /// - i.e. an explicit empty string disables prewarm for this user only).
    ///
    /// When `None`, the pool-level value is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prewarm_query: Option<String>,
    // Pam auth
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_pam_service: Option<String>,
}

impl Default for User {
    fn default() -> User {
        User {
            username: String::from("postgres"),
            password: String::from(""),
            pool_size: 40,
            min_pool_size: None,
            pool_mode: None,
            server_lifetime: None,
            server_username: None,
            server_password: None,
            prewarm_query: None,
            auth_pam_service: None,
        }
    }
}

impl User {
    pub async fn validate(&self) -> Result<(), Error> {
        if self.password.starts_with(JWT_PUB_KEY_PASSWORD_PREFIX) {
            let jwt = parse_jwt_pub_key_password(&self.password)?.expect("prefix checked above");
            validate_jwt_pub_key_file(&jwt.key_filename)?;
        }
        if self.server_password.is_some() && self.server_username.is_none() {
            return Err(Error::BadConfig(
                "server_password requires server_username to be set".to_string(),
            ));
        }
        if let Some(min_pool_size) = self.min_pool_size {
            if min_pool_size > self.pool_size {
                return Err(Error::BadConfig(format!(
                    "min_pool_size of {} cannot be larger than pool_size of {}",
                    min_pool_size, self.pool_size
                )));
            }
        };

        // pool_size = 0 yields a Semaphore::new(0) that
        // never grants - every client checkout for this user hangs
        // for query_wait_timeout then errors. Reject up front so
        // operators see a config error instead of a runtime hang.
        if self.pool_size == 0 {
            return Err(Error::BadConfig(format!(
                "user '{}' pool_size must be >= 1",
                self.username
            )));
        }
        if self.pool_size > MAX_POOL_SIZE {
            return Err(Error::BadConfig(format!(
                "user '{}' pool_size must be <= {MAX_POOL_SIZE}",
                self.username
            )));
        }

        // Validate the per-user prewarm_query override. An empty `Some("")` is a
        // deliberate "disable for this user only" sentinel and is allowed.
        if let Some(ref pw) = self.prewarm_query {
            if !pw.is_empty() {
                if pw.trim().is_empty() {
                    return Err(Error::BadConfig(format!(
                        "user '{}' prewarm_query contains only whitespace; \
                         use \"\" to disable or omit to inherit the pool default",
                        self.username
                    )));
                }
                if pw.len() > 4096 {
                    return Err(Error::BadConfig(format!(
                        "user '{}' prewarm_query exceeds maximum length of 4096 bytes \
                         (got {} bytes)",
                        self.username,
                        pw.len()
                    )));
                }
                // PostgreSQL simple-query frames terminate at the first NUL
                // byte. See pool.rs validators for the matching guard.
                if pw.as_bytes().contains(&b'\0') {
                    return Err(Error::BadConfig(format!(
                        "user '{}' prewarm_query contains a null byte; \
                         PostgreSQL would treat the bytes after it as a new \
                         wire message and new backends would be marked bad \
                         immediately after startup",
                        self.username
                    )));
                }
                validate_prewarm_query_does_not_set_session_state(
                    &format!("user '{}' ", self.username),
                    pw,
                )?;
            }
        }

        // validate `auth_pam_service`. PAM
        // `start()` with empty/whitespace/NUL service-name returns
        // PAM_SYSTEM_ERR; clients see a generic 28P01 with no
        // operator-facing diagnostic. Reject up-front.
        if let Some(ref svc) = self.auth_pam_service {
            if svc.trim().is_empty() {
                return Err(Error::BadConfig(format!(
                    "user '{}' auth_pam_service must not be empty or whitespace only",
                    self.username
                )));
            }
            if svc.as_bytes().contains(&b'\0') {
                return Err(Error::BadConfig(format!(
                    "user '{}' auth_pam_service contains a null byte",
                    self.username
                )));
            }
            if svc.len() > 256 {
                return Err(Error::BadConfig(format!(
                    "user '{}' auth_pam_service exceeds 256 bytes",
                    self.username
                )));
            }
        }

        Ok(())
    }
}
