//! Tests for authentication module.

use super::mocks::{run_test, MockReader, MockWriter};
use super::*;
use openssl::rsa::Rsa;
use std::io::Write;
use tempfile::NamedTempFile;

// Generate a throwaway RSA key pair written to temp PEM files (private, public).
fn generate_temp_rsa_keys() -> (NamedTempFile, NamedTempFile) {
    let rsa = Rsa::generate(2048).unwrap();

    let mut private_file = NamedTempFile::new().unwrap();
    private_file.write_all(&rsa.private_key_to_pem().unwrap()).unwrap();
    private_file.flush().unwrap();

    let mut public_file = NamedTempFile::new().unwrap();
    public_file.write_all(&rsa.public_key_to_pem().unwrap()).unwrap();
    public_file.flush().unwrap();

    (private_file, public_file)
}

// Frame `payload` as a PostgreSQL password message ('p') split into the exact
// chunks `read_password` consumes (type byte, length, body) so MockReader feeds
// it one read at a time.
fn password_message(payload: &[u8]) -> Vec<Vec<u8>> {
    // PostgreSQL password messages are NUL-terminated; vec_to_string relies on it.
    let mut body = payload.to_vec();
    body.push(0);
    let len = (body.len() as i32 + 4).to_be_bytes().to_vec();
    vec![vec![b'p'], len, body]
}

fn admin_client_identifier(username: &str) -> ClientIdentifier {
    ClientIdentifier::new("app", username, "pgdoorman", "127.0.0.1:5432")
}

#[test]
fn hba_trust_rejects_scram_passthrough_without_backend_secret() {
    let scram_verifier = format!("{SCRAM_SHA_256}$4096:salt$storedkey:serverkey");

    assert!(hba_trust_skips_required_scram_passthrough(
        CheckResult::Trust,
        &scram_verifier,
        true,
    ));
    assert!(!hba_trust_skips_required_scram_passthrough(
        CheckResult::Allow,
        &scram_verifier,
        true,
    ));
    assert!(!hba_trust_skips_required_scram_passthrough(
        CheckResult::Trust,
        "md5abcdefabcdefabcdefabcdefabcdefab",
        true,
    ));
    assert!(!hba_trust_skips_required_scram_passthrough(
        CheckResult::Trust,
        &scram_verifier,
        false,
    ));
}

// JWT wrapper happy path: a properly signed token is accepted end-to-end
// (cleartext challenge -> read framed token -> validate against the loaded pub
// key -> username matches). The validation internals are covered in jwt.rs; this
// exercises the authenticate_with_jwt wrapper itself.
#[tokio::test]
async fn authenticate_with_jwt_accepts_valid_token() {
    let (private_file, public_file) = generate_temp_rsa_keys();
    let private_path = private_file.path().to_str().unwrap().to_string();
    let public_path = public_file.path().to_str().unwrap().to_string();

    crate::auth::jwt::load_jwt_pub_key(public_path.clone())
        .await
        .expect("pub key must load");

    let claims = crate::auth::jwt::new_claims_with_scope(
        "test_user".to_string(),
        std::time::Duration::from_secs(3600),
        "issuer".to_string(),
        "audience".to_string(),
    );
    let token = crate::auth::jwt::sign_with_jwt_priv_key(claims, private_path)
        .await
        .expect("token must sign");

    let mut reader = MockReader::new(password_message(token.as_bytes()));
    let mut writer = MockWriter::new();

    let result = authenticate_with_jwt(
        &mut reader,
        &mut writer,
        crate::auth::jwt::JwtVerifierConfig {
            key_filename: public_path,
            issuer: "issuer".to_string(),
            audience: "audience".to_string(),
        },
        "test_user",
        "test_pool",
        "127.0.0.1:5432",
    )
    .await;

    assert!(
        result.is_ok(),
        "a valid signed token must authenticate, got: {result:?}"
    );
}

// JWT wrapper failure path: a token signed for a different audience than the
// verifier expects is rejected with a JWTValidate error and the client receives
// a 28P01 auth-failure response.
#[tokio::test]
async fn authenticate_with_jwt_rejects_wrong_audience_token() {
    let (private_file, public_file) = generate_temp_rsa_keys();
    let private_path = private_file.path().to_str().unwrap().to_string();
    let public_path = public_file.path().to_str().unwrap().to_string();

    crate::auth::jwt::load_jwt_pub_key(public_path.clone())
        .await
        .expect("pub key must load");

    // Signed correctly, but for an audience the verifier does not accept.
    let claims = crate::auth::jwt::new_claims_with_scope(
        "test_user".to_string(),
        std::time::Duration::from_secs(3600),
        "issuer".to_string(),
        "wrong_audience".to_string(),
    );
    let token = crate::auth::jwt::sign_with_jwt_priv_key(claims, private_path)
        .await
        .expect("token must sign");

    let mut reader = MockReader::new(password_message(token.as_bytes()));
    let mut writer = MockWriter::new();

    let result = authenticate_with_jwt(
        &mut reader,
        &mut writer,
        crate::auth::jwt::JwtVerifierConfig {
            key_filename: public_path,
            issuer: "issuer".to_string(),
            audience: "audience".to_string(),
        },
        "test_user",
        "test_pool",
        "127.0.0.1:5432",
    )
    .await;

    assert!(
        matches!(result, Err(Error::JWTValidate(_))),
        "wrong-audience token must be rejected, got: {result:?}"
    );
    let written = writer.get_written().concat();
    assert!(
        String::from_utf8_lossy(&written).contains("28P01"),
        "client must receive a 28P01 auth-failure response"
    );
}

// SCRAM wrapper rejects a misconfigured pool password (not a valid SCRAM
// verifier) before any handshake, returning an error instead of panicking or
// hanging. The SCRAM parsing/crypto internals are covered in scram.rs; a full
// successful handshake needs a real SCRAM client and is out of scope here (the
// `full_test` in scram.rs is commented out for the same reason).
#[tokio::test]
async fn authenticate_with_scram_rejects_invalid_server_secret() {
    let mut reader = MockReader::new(vec![]);
    let mut writer = MockWriter::new();

    let result = authenticate_with_scram(
        &mut reader,
        &mut writer,
        "not-a-valid-scram-verifier",
        "test_user",
        "test_pool",
        "127.0.0.1:5432",
        false, // use_tls
    )
    .await;

    assert!(
        result.is_err(),
        "an invalid SCRAM server secret must be rejected, got: {result:?}"
    );
}

// Test for admin authentication credential check. The md5 challenge salt is
// random inside authenticate_admin, so the verifiable, deterministic part is the
// constant-time response check; admin_password_response_valid exposes it.
#[test]
fn admin_password_response_valid_accepts_correct_and_rejects_wrong() {
    let username = "admin";
    let password = "admin_password";
    let salt = [1u8, 2, 3, 4];

    // The md5 response a client computes for the correct password under the salt.
    let correct = md5_hash_password(username, password, &salt);
    assert!(
        admin_password_response_valid(username, password, &salt, &correct),
        "correct md5 response must be accepted"
    );

    // A response for a different password must be rejected.
    let wrong_password = md5_hash_password(username, "not_the_password", &salt);
    assert!(
        !admin_password_response_valid(username, password, &salt, &wrong_password),
        "md5 response for a different password must be rejected"
    );

    // A response computed under a different salt must be rejected.
    let wrong_salt = md5_hash_password(username, password, &[9u8, 9, 9, 9]);
    assert!(
        !admin_password_response_valid(username, password, &salt, &wrong_salt),
        "md5 response computed under a different salt must be rejected"
    );
}

// An empty admin_password disables the virtual admin console: the admin login
// is rejected outright with a clear reason, instead of `md5_hash_password`
// accepting a client that simply sends an empty password. The pooler itself
// keeps running; only admin access is blocked.
#[test]
fn admin_authentication_rejected_when_admin_password_empty() {
    futures::executor::block_on(run_test(|| async {
        let mut reader = MockReader::new(vec![]);
        let mut writer = MockWriter::new();

        let result = authenticate_admin(&mut reader, &mut writer, "admin", "admin", "").await;

        let err = result.expect_err("empty admin_password must disable the admin console");
        assert!(
            format!("{err}").contains("admin console disabled"),
            "expected admin-console-disabled rejection, got: {err}"
        );
    }));
}

#[test]
fn admin_hba_trust_rejects_wrong_startup_username() {
    futures::executor::block_on(run_test(|| async {
        let mut reader = MockReader::new(vec![]);
        let mut writer = MockWriter::new();
        let mut client_identifier = admin_client_identifier("mallory");
        client_identifier.hba_md5 = CheckResult::Trust;

        let result = authenticate(
            &mut reader,
            &mut writer,
            true,
            &mut client_identifier,
            "pgdoorman",
            "mallory",
        )
        .await;

        assert!(matches!(result, Err(Error::AuthError(msg)) if msg.contains("Invalid admin user")));
        let written = writer.get_written().concat();
        let text = String::from_utf8_lossy(&written);
        assert!(text.contains("28P01"));
    }));
}

#[test]
fn admin_md5_rejects_wrong_startup_username_before_challenge() {
    futures::executor::block_on(run_test(|| async {
        let mut reader = MockReader::new(vec![]);
        let mut writer = MockWriter::new();
        let mut client_identifier = admin_client_identifier("mallory");
        client_identifier.hba_md5 = CheckResult::Allow;

        let result = authenticate(
            &mut reader,
            &mut writer,
            true,
            &mut client_identifier,
            "pgdoorman",
            "mallory",
        )
        .await;

        assert!(matches!(result, Err(Error::AuthError(msg)) if msg.contains("Invalid admin user")));
        let written = writer.get_written().concat();
        assert!(
            !written.starts_with(b"R"),
            "wrong admin user must be rejected before MD5 challenge"
        );
        let text = String::from_utf8_lossy(&written);
        assert!(text.contains("28P01"));
    }));
}

#[test]
fn auth_query_unsupported_verifier_records_failure_and_wire_error() {
    futures::executor::block_on(run_test(|| async {
        let stats = crate::stats::auth_query::AuthQueryStats::default();
        let mut writer = MockWriter::new();

        let err = unsupported_auth_query_verifier_error(
            &mut writer,
            &stats,
            "dynamic_user",
            "app_db",
            "plain-text-secret",
        )
        .await
        .expect("writing the terminal auth error should succeed");

        assert_eq!(stats.auth_failure.load(Ordering::Relaxed), 1);
        assert!(matches!(err, Error::AuthError(msg) if msg.contains("Unsupported password type")));

        let written = writer.get_written().concat();
        let text = String::from_utf8_lossy(&written);
        assert!(text.contains("Unsupported authentication method for auth_query user."));
        assert!(text.contains("28P01"));
    }));
}

#[test]
fn auth_query_dynamic_pool_admission_error_records_failure_and_wire_error() {
    futures::executor::block_on(run_test(|| async {
        let stats = crate::stats::auth_query::AuthQueryStats::default();
        let mut writer = MockWriter::new();
        let cause = Error::AuthError("auth_query: too many dynamic users".to_string());

        let err = auth_query_dynamic_pool_admission_error(
            &mut writer,
            &stats,
            "dynamic_user",
            "app_db",
            cause,
        )
        .await
        .expect("writing the dynamic-pool admission error should succeed");

        assert_eq!(stats.auth_failure.load(Ordering::Relaxed), 1);
        assert!(matches!(err, Error::AuthError(msg) if msg.contains("too many dynamic users")));

        let written = writer.get_written().concat();
        let text = String::from_utf8_lossy(&written);
        assert!(text.contains("Unable to create authenticated dynamic pool."));
        assert!(text.contains("58000"));
    }));
}

#[test]
fn auth_query_dedicated_mode_preserves_authenticated_client_username() {
    let src = include_str!("mod.rs");
    let start = src
        .find("Some(ref shared_pool_id) => {")
        .expect("auth_query dedicated branch not found");
    let body = &src[start..];
    let end = body
        .find("None => {")
        .expect("auth_query passthrough branch should follow dedicated branch");
    let dedicated = &body[..end];

    assert!(
        !dedicated.contains("client_identifier.username = shared_pool_id.user.clone()"),
        "auth_query dedicated mode must not overwrite the authenticated client username with server_user"
    );
    assert!(
        dedicated.contains("pool_user"),
        "auth_query dedicated mode must return the shared backend pool user separately"
    );
}

#[test]
fn static_auth_path_reroutes_dedicated_auth_query_shared_pool_user() {
    let src = include_str!("mod.rs");
    let start = src
        .find("async fn authenticate_normal_user")
        .expect("normal auth path should exist");
    let body = &src[start..];
    let end = body
        .find("async fn authenticate_with_pam")
        .expect("PAM helper should follow normal auth path");
    let body = &body[..end];
    let static_pool_branch = body
        .find("Some(pool) => {")
        .expect("normal auth path must branch on an existing pool");
    let before_accepting_pool = body[static_pool_branch..]
        .find("\n            pool\n")
        .map(|offset| static_pool_branch + offset)
        .expect("normal auth path should eventually accept a static pool");
    let branch = &body[static_pool_branch..before_accepting_pool];

    assert!(
        branch.contains("is_auth_query_shared_pool("),
        "normal static auth must detect auth_query dedicated shared pools before \
         HBA trust can authenticate the server_user directly"
    );
    assert!(
        branch.contains("return try_auth_query("),
        "direct login as auth_query.server_user must be routed through auth_query, \
         not accepted as an ordinary static user"
    );
}

#[test]
fn pam_authentication_is_blocking_isolated() {
    let src = include_str!("mod.rs");
    let start = src
        .find("async fn authenticate_with_pam")
        .expect("PAM auth path should exist");
    let body = &src[start..];
    let end = body
        .find("async fn authenticate_with_jwt")
        .expect("JWT auth path should follow PAM auth");
    let body = &body[..end];

    assert!(
        body.contains("PAM_AUTH_SEMAPHORE"),
        "PAM auth must be bounded by a semaphore before entering blocking code"
    );
    assert!(
        body.contains("tokio::task::spawn_blocking"),
        "PAM auth must not run synchronous PAM calls on Tokio workers"
    );
    assert!(
        body.contains("tokio::time::timeout(PAM_AUTH_TIMEOUT"),
        "PAM auth must have an explicit timeout around blocking isolation"
    );
    let spawn_idx = body
        .find("tokio::task::spawn_blocking")
        .expect("PAM auth must enter blocking isolation");
    let pam_call_idx = body[spawn_idx..]
        .find("pam_auth(&service, &username, &password)")
        .map(|offset| spawn_idx + offset)
        .expect("PAM auth call must stay inside the blocking worker");
    assert!(
        body[spawn_idx..pam_call_idx].contains("let _permit = permit;"),
        "the PAM concurrency permit must stay owned by the uncancellable blocking worker"
    );
}
