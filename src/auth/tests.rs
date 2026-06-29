//! Tests for authentication module.

use super::mocks::{run_test, MockReader, MockWriter};
use super::*;

// Mock for get_config and get_pool
fn mock_get_config() -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin_password".to_string();
    config
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

// Tests for JWT authentication
#[test]
fn test_jwt_authentication() {
    let _result = run_test(|| async {
        let mut reader = MockReader::new(vec![b"valid_token".to_vec()]);
        let mut writer = MockWriter::new();

        let result = authenticate_with_jwt(
            &mut reader,
            &mut writer,
            crate::auth::jwt::JwtVerifierConfig {
                key_filename: "jwt_pub_key".to_string(),
                issuer: "issuer".to_string(),
                audience: "audience".to_string(),
            },
            "test_user",
            "test_pool",
            "127.0.0.1:5432",
        )
        .await;

        assert!(result.is_ok());

        result
    });
}

#[test]
fn test_jwt_authentication_failure() {
    let _result = run_test(|| async {
        let mut reader = MockReader::new(vec![b"invalid_token".to_vec()]);
        let mut writer = MockWriter::new();

        let result = authenticate_with_jwt(
            &mut reader,
            &mut writer,
            crate::auth::jwt::JwtVerifierConfig {
                key_filename: "jwt_pub_key".to_string(),
                issuer: "issuer".to_string(),
                audience: "audience".to_string(),
            },
            "test_user",
            "test_pool",
            "127.0.0.1:5432",
        )
        .await;

        assert!(result.is_err());
        if let Err(Error::JWTValidate(ref msg)) = result {
            assert!(msg.contains("Invalid JWT token"));
        } else {
            panic!("Expected JWTValidate error");
        }

        result
    });
}

// Test for SCRAM authentication
#[test]
fn test_scram_authentication() {
    let _result = run_test(|| async {
        // For SCRAM authentication, we need to mock the client first message and final message
        let client_first_message =
            format!("{SCRAM_SHA_256}\\0\\0\\0\\0 n,,n=,r=5DAkMQDUZpG/3GcwewTYJZbD");
        let client_final_message = "c=biws,r=5DAkMQDUZpG/3GcwewTYJZbDrandom,p=validproof";

        let mut reader = MockReader::new(vec![
            client_first_message.as_bytes().to_vec(),
            client_final_message.as_bytes().to_vec(),
        ]);
        let mut writer = MockWriter::new();

        let server_secret = format!("{SCRAM_SHA_256}$4096:salt$storedkey:serverkey");

        let result = authenticate_with_scram(
            &mut reader,
            &mut writer,
            &server_secret,
            "test_user",
            "test_pool",
            "127.0.0.1:5432",
            false, // use_tls: legacy plain-TCP scenario for this fixture
        )
        .await;
        assert!(result.is_ok());
    });
}

// Test for admin authentication
#[test]
fn test_admin_authentication() {
    let _result = run_test(|| async {
        // Mock the password response for admin authentication
        let config = mock_get_config();
        let salt = [1, 2, 3, 4];
        let password_hash = md5_hash_password(
            &config.general.admin_username,
            &config.general.admin_password,
            &salt,
        );

        let mut reader = MockReader::new(vec![password_hash]);
        let mut writer = MockWriter::new();

        let result = authenticate_admin(
            &mut reader,
            &mut writer,
            "admin",
            &config.general.admin_username,
            &config.general.admin_password,
        )
        .await;

        // This test might fail due to the need for more sophisticated mocking
        // of the get_config function
        assert!(result.is_ok());
    });
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
