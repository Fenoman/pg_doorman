use bytes::{BufMut, BytesMut};
use log::error;
use tokio::io::AsyncReadExt;

use crate::auth::jwt::{new_claims, sign_with_jwt_priv_key};
use crate::auth::scram_client::ScramSha256;
use crate::config::{BackendAuthMethod, User};
use crate::errors::{Error, ServerIdentifier};
use crate::messages::constants::*;
use crate::messages::{md5_hash_password, md5_hash_second_pass, write_all_flush};

use super::stream::StreamInner;

/// Gate the wire `len` field for backend AuthenticationResponse subtypes
/// BEFORE any `vec![0u8; (len - 8) as usize]` allocation. Without this:
/// - `len < 8` wraps the subtraction on cast to usize, producing
///   `usize::MAX` and aborting the worker via allocator failure
///   (bypasses the panic hook because allocator-abort calls
///   `__rust_alloc_error_handler`).
/// - `len > MAX_AUTH_FRAME_LEN` lets a hostile / MITM backend reserve
///   multi-GiB while pg_doorman blocks in `read_exact` until the
///   socket closes - workers wedged, CURRENT_MEMORY budget bypassed
///   because the auth path does not use `MemoryReservation`.
///
/// PostgreSQL never sends an AuthenticationResponse beyond a few KiB
/// for SCRAM-SHA-256-PLUS challenge/response, MD5 salt, or clear-
/// password requests; 64 KiB is a generous upper bound that rejects
/// pathological frames while accepting every real one.
const MAX_AUTH_FRAME_LEN: i32 = 65_536;

#[inline]
fn validate_auth_frame_len(
    len: i32,
    server_identifier: &ServerIdentifier,
    label: &'static str,
) -> Result<usize, Error> {
    if len < 8 {
        return Err(Error::ServerAuthError(
            format!(
                "{label}: frame length {len} smaller than 8-byte header - \
                 possible MITM or corrupted backend"
            ),
            server_identifier.clone(),
        ));
    }
    if len > MAX_AUTH_FRAME_LEN {
        return Err(Error::ServerAuthError(
            format!("{label}: frame length {len} exceeds {MAX_AUTH_FRAME_LEN}-byte cap"),
            server_identifier.clone(),
        ));
    }
    Ok((len - 8) as usize)
}

#[inline]
fn validate_no_payload_auth_frame_len(
    len: i32,
    server_identifier: &ServerIdentifier,
    label: &'static str,
) -> Result<(), Error> {
    if len != 8 {
        return Err(Error::ServerAuthError(
            format!("{label}: frame length must be 8 bytes, got len={len}"),
            server_identifier.clone(),
        ));
    }
    Ok(())
}

/// Handles authentication during server startup.
/// Processes various authentication methods: SASL, MD5, clear password.
pub(crate) async fn handle_authentication(
    stream: &mut StreamInner,
    auth_code: i32,
    len: i32,
    user: &User,
    scram_client_auth: &mut Option<ScramSha256>,
    server_identifier: &ServerIdentifier,
    backend_auth: Option<&BackendAuthMethod>,
) -> Result<(), Error> {
    match auth_code {
        AUTHENTICATION_SUCCESSFUL => {
            validate_no_payload_auth_frame_len(len, server_identifier, "AuthenticationOk")?;
            Ok(())
        }

        // SASL authentication
        SASL => {
            let scram = scram_client_auth.as_mut().ok_or_else(|| {
                Error::ServerAuthError(
                    "server wants sasl auth, but it is not configured".into(),
                    server_identifier.clone(),
                )
            })?;

            let sasl_len = validate_auth_frame_len(len, server_identifier, "SASL")?;
            // Need at least mechanism-name + trailing NUL (2 bytes
            // minimum to safely compute `sasl_len - 2` below).
            if sasl_len < 2 {
                return Err(Error::ServerAuthError(
                    format!("SASL frame body {sasl_len} bytes too small for mechanism+null"),
                    server_identifier.clone(),
                ));
            }
            let mut sasl_auth = vec![0u8; sasl_len];
            stream.read_exact(&mut sasl_auth).await.map_err(|_| {
                Error::ServerStartupError(
                    "Failed to read SASL authentication message from server".into(),
                    server_identifier.clone(),
                )
            })?;

            let sasl_type = String::from_utf8_lossy(&sasl_auth[..sasl_len - 2]);
            if !sasl_type.contains(SCRAM_SHA_256) {
                error!(
                    "[{}@{}] unsupported SCRAM version: {sasl_type}",
                    server_identifier.username, server_identifier.pool_name
                );
                return Err(Error::ServerAuthError(
                    format!("Unsupported SCRAM version: {sasl_type}"),
                    server_identifier.clone(),
                ));
            }

            // Generate and send client message
            let sasl_response = scram.message();
            let mut res = BytesMut::new();
            res.put_u8(b'p');
            res.put_i32(4 + SCRAM_SHA_256.len() as i32 + 1 + 4 + sasl_response.len() as i32);
            res.put_slice(format!("{SCRAM_SHA_256}\0").as_bytes());
            res.put_i32(sasl_response.len() as i32);
            res.put(sasl_response);
            write_all_flush(stream, &res).await?;
            Ok(())
        }

        // SASL continuation
        SASL_CONTINUE => {
            let body_len = validate_auth_frame_len(len, server_identifier, "SASL_CONTINUE")?;
            let mut sasl_data = vec![0u8; body_len];
            stream.read_exact(&mut sasl_data).await.map_err(|_| {
                Error::ServerStartupError(
                    "Failed to read SASL continuation message from server".into(),
                    server_identifier.clone(),
                )
            })?;

            // typed-error replacement for `unwrap()` -
            // backend may emit SASL_CONTINUE before SASL was ever
            // negotiated.
            let scram = scram_client_auth.as_mut().ok_or_else(|| {
                Error::ServerAuthError(
                    "SASL_CONTINUE before SASL handshake started".into(),
                    server_identifier.clone(),
                )
            })?;
            let msg = BytesMut::from(&sasl_data[..]);
            let sasl_response = scram.update(&msg)?;

            let mut res = BytesMut::new();
            res.put_u8(b'p');
            res.put_i32(4 + sasl_response.len() as i32);
            res.put(sasl_response);
            write_all_flush(stream, &res).await?;
            Ok(())
        }

        // SASL final
        SASL_FINAL => {
            let body_len = validate_auth_frame_len(len, server_identifier, "SASL_FINAL")?;
            let mut sasl_final = vec![0u8; body_len];
            stream.read_exact(&mut sasl_final).await.map_err(|_| {
                Error::ServerStartupError(
                    "failed to read SASL final message from server".into(),
                    server_identifier.clone(),
                )
            })?;

            // typed-error replacement for `unwrap()`.
            let scram = scram_client_auth.as_mut().ok_or_else(|| {
                Error::ServerAuthError(
                    "SASL_FINAL before SASL handshake started".into(),
                    server_identifier.clone(),
                )
            })?;
            scram.finish(&BytesMut::from(&sasl_final[..]))?;
            Ok(())
        }

        // Clear password authentication
        AUTHENTICATION_CLEAR_PASSWORD => {
            validate_no_payload_auth_frame_len(
                len,
                server_identifier,
                "AuthenticationCleartextPassword",
            )?;
            if user.server_username.is_none() || user.server_password.is_none() {
                error!(
                    "[{}@{}] clear password authentication requested by server but not configured",
                    server_identifier.username, server_identifier.pool_name,
                );
                return Err(Error::ServerAuthError(
                    "server wants clear password authentication, but auth for this server is not configured".into(),
                    server_identifier.clone(),
                ));
            }

            let server_password = user.server_password.as_ref().unwrap().clone();
            let server_username = user.server_username.as_ref().unwrap().clone();

            if !server_password.starts_with(JWT_PRIV_KEY_PASSWORD_PREFIX) {
                return Err(Error::ServerAuthError(
                    "plain password is not supported".into(),
                    server_identifier.clone(),
                ));
            }

            // Generate JWT token
            let claims = new_claims(server_username, std::time::Duration::from_secs(120));
            let token = sign_with_jwt_priv_key(
                claims,
                server_password
                    .strip_prefix(JWT_PRIV_KEY_PASSWORD_PREFIX)
                    .unwrap()
                    .to_string(),
            )
            .await
            .map_err(|err| Error::ServerAuthError(err.to_string(), server_identifier.clone()))?;

            let mut password_response = BytesMut::new();
            password_response.put_u8(b'p');
            password_response.put_i32(token.len() as i32 + 4 + 1);
            password_response.put_slice(token.as_bytes());
            password_response.put_u8(b'\0');
            stream.write_all(&password_response).await.map_err(|err| {
                Error::ServerAuthError(
                    format!("jwt authentication on the server failed: {err:?}"),
                    server_identifier.clone(),
                )
            })?;
            Ok(())
        }

        // MD5 password authentication
        MD5_ENCRYPTED_PASSWORD => {
            // canonical MD5 'R' frame is exactly 12
            // bytes (header 8 + 4-byte salt). The previous shape used
            // `read_buf` which returns whatever the socket has
            // available - a partial read across a TCP segment
            // boundary could yield 0-3 bytes of salt, then the MD5
            // hash was computed against a short / empty salt,
            // producing reproducible client-key material on the wire.
            if len != 12 {
                return Err(Error::ServerAuthError(
                    format!("MD5 'R' frame must be 12 bytes, got len={len}"),
                    server_identifier.clone(),
                ));
            }
            let mut salt = BytesMut::zeroed(4);
            stream.read_exact(&mut salt[..]).await.map_err(|err| {
                Error::ServerAuthError(
                    format!("md5 authentication on the server: {err:?}"),
                    server_identifier.clone(),
                )
            })?;

            // Check for pass-the-hash first (auth_query passthrough)
            let password_hash = if let Some(BackendAuthMethod::Md5PassTheHash(md5_hash)) =
                backend_auth
            {
                let hash_hex = md5_hash.strip_prefix("md5").unwrap_or(md5_hash);
                md5_hash_second_pass(hash_hex, salt.as_mut())
            } else {
                // Static user: derive from server_username/server_password
                if user.server_username.is_none() || user.server_password.is_none() {
                    error!(
                        "[{}@{}] MD5 authentication requested by server but not configured",
                        server_identifier.username, server_identifier.pool_name,
                    );
                    return Err(Error::ServerAuthError(
                            "server wants md5 authentication, but auth for this server is not configured"
                                .into(),
                            server_identifier.clone(),
                        ));
                }

                let server_username = user.server_username.as_ref().unwrap();
                let server_password = user.server_password.as_ref().unwrap();
                md5_hash_password(
                    server_username.as_str(),
                    server_password.as_str(),
                    salt.as_mut(),
                )
            };

            let mut password_response = BytesMut::new();
            password_response.put_u8(b'p');
            password_response.put_i32(password_hash.len() as i32 + 4);
            password_response.put_slice(&password_hash);
            stream.write_all(&password_response).await.map_err(|err| {
                Error::ServerAuthError(
                    format!("md5 authentication on the server failed: {err:?}"),
                    server_identifier.clone(),
                )
            })?;
            Ok(())
        }

        _ => {
            error!(
                "[{}@{}] unsupported auth method: code={}",
                server_identifier.username, server_identifier.pool_name, auth_code
            );
            Err(Error::ServerAuthError(
                "authentication on the server is not supported".into(),
                server_identifier.clone(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_identifier() -> ServerIdentifier {
        ServerIdentifier::new("user".to_string(), "db", "pool")
    }

    #[test]
    fn no_payload_auth_frame_len_must_be_exact() {
        let id = server_identifier();
        validate_no_payload_auth_frame_len(8, &id, "AuthenticationOk").unwrap();

        let err = validate_no_payload_auth_frame_len(12, &id, "AuthenticationOk")
            .expect_err("surplus bytes must be rejected");
        assert!(
            format!("{err}").contains("must be 8 bytes"),
            "unexpected error: {err}"
        );
    }
}
