//! Tests for configuration module.

#![allow(clippy::field_reassign_with_default)]

use super::*;
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use openssl::x509::{X509NameBuilder, X509};
use serial_test::serial;
use std::io::Write;
use tempfile::NamedTempFile;

// Helper function to create a temporary config file for testing
fn create_temp_config() -> NamedTempFile {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432
idle_timeout = 40000

[[pools.example_db.users]]
username = "example_user_1"
password = "password1"
pool_size = 40
pool_mode = "transaction"

[[pools.example_db.users]]
username = "example_user_2"
password = "SCRAM-SHA-256$4096:p2j/1lMdQF6r1dD9I9f7PQ==$H3xt5yh7lwSq9zUPYwHovRu3FyUCCXchG/skydJRa9o=:5xU6Wj/GNg3UnN2uQIx3ezx7uZyzGeM5NrvSJRIxnlw="
pool_size = 20

[pools.test_db1]
server_host = "localhost"
server_port = 5432

[pools.test_db2]
server_host = "localhost"
server_port = 5432

[pools.test_db3]
server_host = "localhost"
server_port = 5432
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();
    temp_file
}

fn write_rsa_public_key(path: &std::path::Path) {
    let rsa = Rsa::generate(2048).unwrap();
    let public_pem = rsa.public_key_to_pem().unwrap();
    std::fs::write(path, public_pem).unwrap();
}

fn write_rsa_keypair(public_path: &std::path::Path, private_path: &std::path::Path) {
    let rsa = Rsa::generate(2048).unwrap();
    let public_pem = rsa.public_key_to_pem().unwrap();
    let private_pem = rsa.private_key_to_pem().unwrap();
    std::fs::write(public_path, public_pem).unwrap();
    std::fs::write(private_path, private_pem).unwrap();
}

fn write_self_signed_tls_identity(cert_path: &std::path::Path, key_path: &std::path::Path) {
    let rsa = Rsa::generate(2048).unwrap();
    let key = PKey::from_rsa(rsa).unwrap();

    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "localhost").unwrap();
    let name = name.build();

    let mut serial = BigNum::new().unwrap();
    serial.rand(64, MsbOption::MAYBE_ZERO, false).unwrap();
    let serial = serial.to_asn1_integer().unwrap();

    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&serial).unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    let not_before = Asn1Time::days_from_now(0).unwrap();
    let not_after = Asn1Time::days_from_now(1).unwrap();
    cert.set_not_before(&not_before).unwrap();
    cert.set_not_after(&not_after).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();

    std::fs::write(cert_path, cert.build().to_pem().unwrap()).unwrap();
    std::fs::write(key_path, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
}

fn ensure_test_log_controller() {
    if crate::app::log_level::get_log_level() != "unknown" {
        return;
    }

    struct NoopLogger;

    impl log::Log for NoopLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, _record: &log::Record) {}

        fn flush(&self) {}
    }

    crate::app::log_level::LogLevelController::new(Box::new(NoopLogger), log::LevelFilter::Info)
        .register();
}

fn use_strong_admin_password(config: &mut Config) {
    config.general.admin_password = "admin_password".to_string();
}

#[tokio::test]
#[serial]
async fn test_config() {
    let temp_file = create_temp_config();
    let file_path = temp_file.path().to_str().unwrap();

    parse(file_path).await.unwrap();

    assert_eq!(get_config().pools.len(), 4);
    assert_eq!(get_config().pools["example_db"].idle_timeout, Some(40000));
    assert_eq!(
        get_config().pools["example_db"].users[0].username,
        "example_user_1"
    );
    assert_eq!(
        get_config().pools["example_db"].users[1].password,
        "SCRAM-SHA-256$4096:p2j/1lMdQF6r1dD9I9f7PQ==$H3xt5yh7lwSq9zUPYwHovRu3FyUCCXchG/skydJRa9o=:5xU6Wj/GNg3UnN2uQIx3ezx7uZyzGeM5NrvSJRIxnlw="
    );
    assert_eq!(get_config().pools["example_db"].users[1].pool_size, 20);
    assert_eq!(
        get_config().pools["example_db"].users[1].username,
        "example_user_2"
    );
    assert_eq!(get_config().pools["example_db"].users[0].pool_size, 40);
    assert_eq!(
        get_config().pools["example_db"].users[0].pool_mode,
        Some(PoolMode::Transaction)
    );
}

#[tokio::test]
#[serial]
async fn test_serialize_configs() {
    let temp_file = create_temp_config();
    let file_path = temp_file.path().to_str().unwrap();

    parse(file_path).await.unwrap();
    print!("{}", toml::to_string(&get_config()).unwrap());
}

/// cover the change `User::validate()` guard against
/// `pool_size = 0`, which would otherwise yield a
/// `Semaphore::new(0)` that never grants and every client checkout
/// for that user hangs until `query_wait_timeout`.
#[tokio::test]
async fn test_user_pool_size_zero_is_rejected() {
    let user = User {
        username: "alice".to_string(),
        password: "secret".to_string(),
        pool_size: 0,
        ..User::default()
    };
    let err = user
        .validate()
        .await
        .expect_err("pool_size=0 must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("pool_size must be >= 1"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn test_user_pool_size_upper_bound_is_rejected() {
    let user = User {
        username: "alice".to_string(),
        password: "secret".to_string(),
        pool_size: u32::MAX,
        ..User::default()
    };

    let err = user
        .validate()
        .await
        .expect_err("oversized pool_size must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("pool_size must be <="),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn test_validate_valid_config() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Add a pool with a user
    let mut pool = Pool::default();
    let user = User {
        username: "test_user".to_string(),
        password: "test_password".to_string(),
        pool_size: 50, // Greater than virtual_pool_count
        ..User::default()
    };
    pool.users.push(user);
    config.pools.insert("test_pool".to_string(), pool);

    // Set valid TLS rate limit
    config.general.tls_rate_limit_per_second = 100;

    // Set valid prepared statements config
    config.general.prepared_statements = true;
    config.general.prepared_statements_cache_size = 1024;

    let result = config.validate().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_validate_warns_but_allows_default_admin_password_on_wildcard_open_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.hba.clear();
    config.general.pg_hba = None;

    let mut pool = Pool::default();
    pool.users.push(User {
        username: "test_user".to_string(),
        password: "test_password".to_string(),
        pool_size: 10,
        ..User::default()
    });
    config.pools.insert("test_pool".to_string(), pool);

    // The dangerous combination is still detected so the warning fires.
    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "published admin/admin on wildcard open HBA must be detected"
    );
    // But it no longer blocks startup: validate() succeeds (with a logged warning).
    config
        .validate()
        .await
        .expect("default admin password must warn, not reject");
}

#[tokio::test]
async fn test_empty_admin_password_allowed_and_not_flagged_on_wildcard_open_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = String::new();
    config.general.hba.clear();
    config.general.pg_hba = None;

    let mut pool = Pool::default();
    pool.users.push(User {
        username: "test_user".to_string(),
        password: "test_password".to_string(),
        pool_size: 10,
        ..User::default()
    });
    config.pools.insert("test_pool".to_string(), pool);

    // An empty admin_password disables the admin console (auth::authenticate_admin
    // rejects every login), so it is not a remote-exposure concern and is not
    // flagged ...
    assert!(
        !default_admin_password_exposes_remote_tcp_admin(&config.general),
        "empty admin_password must not be flagged as remote-exposed (admin console is disabled)"
    );
    // ... and it no longer blocks startup.
    config
        .validate()
        .await
        .expect("empty admin password must not block startup");
}

#[tokio::test]
async fn test_empty_admin_password_not_flagged_on_loopback_listener() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "127.0.0.1".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = String::new();
    config.general.hba.clear();
    config.general.pg_hba = None;

    // Loopback-only listener keeps the empty-password admin console off remote
    // TCP, so it is not flagged and validate() succeeds without a warning.
    assert!(
        !default_admin_password_exposes_remote_tcp_admin(&config.general),
        "empty admin_password on loopback listener must not be flagged"
    );
    config
        .validate()
        .await
        .expect("loopback empty admin password must validate");
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_wildcard_remote_pg_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.pg_hba = Some(crate::auth::hba::PgHba::from_content(
        "host all all 0.0.0.0/0 md5",
    ));

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "remote admin/admin pg_hba on wildcard listener must be flagged"
    );
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_resolved_wildcard_host() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.hba.clear();
    config.general.pg_hba = None;

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "host aliases resolving to wildcard must be flagged for published admin/admin"
    );
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_non_loopback_ipv4_listener() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "192.0.2.10".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.hba.clear();
    config.general.pg_hba = None;

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "non-loopback listener plus open HBA must be flagged for published admin/admin"
    );
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_non_loopback_ipv6_listener() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "[2001:db8::10]".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.hba.clear();
    config.general.pg_hba = None;

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "non-loopback IPv6 listener plus open HBA must be flagged for published admin/admin"
    );
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_private_admin_pg_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.pg_hba = Some(crate::auth::hba::PgHba::from_content(
        "host pgdoorman admin 10.0.0.0/8 md5",
    ));

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "private-CIDR admin/admin pg_hba on wildcard listener must be flagged"
    );
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_half_range_admin_pg_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.pg_hba = Some(crate::auth::hba::PgHba::from_content(
        "host pgdoorman admin 0.0.0.0/1 md5",
    ));

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "half-range admin pg_hba contains remote peers and must be flagged"
    );
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_private_legacy_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.hba = vec!["10.0.0.0/8".parse().unwrap()];
    config.general.pg_hba = None;

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "private-CIDR legacy hba must be flagged for published admin/admin on wildcard listener"
    );
}

#[tokio::test]
async fn test_default_admin_password_exposure_detected_on_half_range_legacy_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.hba = vec!["0.0.0.0/1".parse().unwrap()];
    config.general.pg_hba = None;

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "half-range legacy hba contains remote peers and must be flagged"
    );
}

#[tokio::test]
async fn test_generated_admin_placeholder_exposure_detected_on_wildcard_open_hba() {
    let mut config = Config::default();
    config.general.host = "0.0.0.0".to_string();
    config.general.hba.clear();
    config.general.pg_hba = None;

    assert!(
        default_admin_password_exposes_remote_tcp_admin(&config.general),
        "generated admin placeholder on wildcard open HBA must be flagged"
    );
}

#[tokio::test]
async fn test_default_admin_password_not_flagged_on_wildcard_loopback_pg_hba() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.host = "0.0.0.0".to_string();
    config.general.admin_username = "admin".to_string();
    config.general.admin_password = "admin".to_string();
    config.general.pg_hba = Some(crate::auth::hba::PgHba::from_content(
        "host all all 127.0.0.1/32 trust\nhost all all ::1/128 trust",
    ));

    // Loopback-only pg_hba keeps published admin/admin off remote TCP, so it is
    // not flagged and validate() succeeds without a warning.
    assert!(
        !default_admin_password_exposes_remote_tcp_admin(&config.general),
        "loopback-only pg_hba must not be flagged as remote-exposed"
    );
    config
        .validate()
        .await
        .expect("loopback-only pg_hba config must validate");
}

#[test]
fn general_default_admin_password_is_not_published_secret() {
    assert_ne!(
        General::default().admin_password,
        "admin",
        "generated/default configs must not publish admin/admin"
    );
}

#[tokio::test]
async fn test_validate_web_log_tap_max_entries_cap() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.web.log_tap_max_entries = web::MAX_LOG_TAP_ENTRIES + 1;

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("log_tap_max_entries"),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about log_tap_max_entries cap");
    }
}

#[tokio::test]
async fn test_validate_web_host_rejects_dns_name_when_enabled() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.web.enabled = true;
    config.web.host = "localhost".to_string();

    let err = config
        .validate()
        .await
        .expect_err("web.host DNS names must be rejected because web listener parses SocketAddr");
    let msg = format!("{err}");
    assert!(
        msg.contains("[web].host") && msg.contains("SocketAddr"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn test_validate_web_host_rejects_invalid_socket_addr_when_enabled() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.web.enabled = true;
    config.web.host = "bad host".to_string();

    let err = config
        .validate()
        .await
        .expect_err("invalid web.host must be rejected before web listener bind");
    let msg = format!("{err}");
    assert!(
        msg.contains("[web].host") && msg.contains("SocketAddr"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn test_validate_general_host_resolves_for_listener() {
    let mut config = Config::default();
    config.general.host = "not a valid bind host".to_string();

    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("general.host"));
        assert!(msg.contains("not a valid bind host"));
    } else {
        panic!("Expected BadConfig error about general.host");
    }
}

#[tokio::test]
async fn test_validate_max_blocking_threads_nonzero() {
    let mut config = Config::default();
    config.general.max_blocking_threads = Some(0);

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("general.max_blocking_threads"));
    } else {
        panic!("Expected BadConfig error about general.max_blocking_threads");
    }
}

#[tokio::test]
async fn test_validate_max_concurrent_creates_nonzero() {
    let mut config = Config::default();
    config.general.max_concurrent_creates = 0;

    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("max_concurrent_creates"));
    } else {
        panic!("Expected BadConfig error about max_concurrent_creates");
    }
}

#[tokio::test]
async fn test_validate_retain_connections_time_nonzero() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.retain_connections_time = crate::config::duration::Duration(0);

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("general.retain_connections_time"));
    } else {
        panic!("Expected BadConfig error about general.retain_connections_time");
    }
}

#[tokio::test]
async fn test_validate_default_backlog_rejects_u32_overflow_max_connections() {
    let mut config = Config::default();
    config.general.backlog = 0;
    config.general.max_connections = u32::MAX as u64 + 1;

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.max_connections must be <= u32::MAX when general.backlog is 0"),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about default backlog max_connections overflow");
    }
}

#[tokio::test]
async fn test_validate_worker_threads_has_upper_bound() {
    let mut config = Config::default();
    config.general.worker_threads = MAX_WORKER_THREADS + 1;

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.worker_threads must be <= "),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about worker_threads upper bound");
    }
}

#[tokio::test]
async fn test_validate_message_size_to_be_stream_below_protocol_max() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.message_size_to_be_stream =
        ByteSize::from_bytes(crate::messages::MAX_MESSAGE_SIZE as u64);

    let err = config
        .validate()
        .await
        .expect_err("message_size_to_be_stream >= protocol max must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("general.message_size_to_be_stream"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn test_validate_pooler_check_query_rejects_embedded_nul() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.pooler_check_query = "select\0 1".to_string();

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.pooler_check_query"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("NUL"), "unexpected error message: {msg}");
    } else {
        panic!("Expected BadConfig error about pooler_check_query NUL byte");
    }
}

#[tokio::test]
async fn test_validate_general_patroni_api_urls_rejects_userinfo() {
    let mut config = config_with_single_pool_user();
    config.general.patroni_api_urls = Some(vec![
        "https://patroni_user:secret@patroni.local:8008".to_string()
    ]);

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.patroni_api_urls"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("userinfo"), "unexpected error message: {msg}");
        assert!(
            !msg.contains("secret"),
            "Patroni URL credentials must not be echoed in config errors: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about Patroni URL userinfo");
    }
}

#[tokio::test]
async fn test_validate_general_patroni_api_urls_rejects_empty_authority() {
    let mut config = config_with_single_pool_user();
    config.general.patroni_api_urls = Some(vec!["http://".to_string()]);

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.patroni_api_urls"),
            "unexpected error message: {msg}"
        );
        assert!(
            msg.contains("host") || msg.contains("URL"),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about Patroni URL host");
    }
}

#[tokio::test]
async fn test_validate_general_patroni_api_urls_rejects_query_or_fragment() {
    for url in [
        "https://patroni.local:8008/cluster?token=secret",
        "https://patroni.local:8008/cluster#secret",
    ] {
        let mut config = config_with_single_pool_user();
        config.general.patroni_api_urls = Some(vec![url.to_string()]);

        let result = config.validate().await;

        if let Err(Error::BadConfig(msg)) = result {
            assert!(
                msg.contains("general.patroni_api_urls"),
                "unexpected error message: {msg}"
            );
            assert!(
                msg.contains("query") || msg.contains("fragment"),
                "unexpected error message: {msg}"
            );
            assert!(
                !msg.contains("secret"),
                "Patroni URL query/fragment payload must not be echoed in config errors: {msg}"
            );
        } else {
            panic!("Expected BadConfig error about Patroni URL query/fragment");
        }
    }
}

#[tokio::test]
async fn test_validate_pool_patroni_api_urls_rejects_userinfo() {
    let mut pool = Pool::default();
    pool.patroni_api_urls = Some(vec!["http://token@127.0.0.1:8008".to_string()]);

    let result = pool.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("patroni_api_urls"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("userinfo"), "unexpected error message: {msg}");
        assert!(
            !msg.contains("token"),
            "Patroni URL credentials must not be echoed in config errors: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about Patroni URL userinfo");
    }
}

#[tokio::test]
async fn test_validate_pool_patroni_api_urls_rejects_whitespace() {
    let mut pool = Pool::default();
    pool.patroni_api_urls = Some(vec!["http://127.0.0.1:8008 /cluster".to_string()]);

    let result = pool.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("patroni_api_urls"),
            "unexpected error message: {msg}"
        );
        assert!(
            msg.contains("whitespace") || msg.contains("URL"),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about Patroni URL whitespace");
    }
}

#[tokio::test]
async fn test_validate_pool_patroni_api_urls_rejects_query_or_fragment() {
    for url in [
        "https://patroni.local:8008/cluster?token=secret",
        "https://patroni.local:8008/cluster#secret",
    ] {
        let mut pool = Pool::default();
        pool.patroni_api_urls = Some(vec![url.to_string()]);

        let result = pool.validate().await;

        if let Err(Error::BadConfig(msg)) = result {
            assert!(
                msg.contains("patroni_api_urls"),
                "unexpected error message: {msg}"
            );
            assert!(
                msg.contains("query") || msg.contains("fragment"),
                "unexpected error message: {msg}"
            );
            assert!(
                !msg.contains("secret"),
                "Patroni URL query/fragment payload must not be echoed in config errors: {msg}"
            );
        } else {
            panic!("Expected BadConfig error about Patroni URL query/fragment");
        }
    }
}

#[tokio::test]
async fn test_validate_web_sso_proxy_url_rejects_userinfo() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.web.sso_proxy_url = Some("https://sso_user:secret@sso.example/start".to_string());

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("[web].sso_proxy_url"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("userinfo"), "unexpected error message: {msg}");
        assert!(
            !msg.contains("secret"),
            "SSO proxy URL credentials must not be echoed in config errors: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about SSO proxy URL userinfo");
    }
}

#[tokio::test]
async fn test_validate_web_sso_proxy_url_rejects_query_or_fragment() {
    for url in [
        "https://sso.example/start?token=secret",
        "https://sso.example/start#secret",
    ] {
        let mut config = Config::default();
        use_strong_admin_password(&mut config);
        config.web.sso_proxy_url = Some(url.to_string());

        let result = config.validate().await;

        if let Err(Error::BadConfig(msg)) = result {
            assert!(
                msg.contains("[web].sso_proxy_url"),
                "unexpected error message: {msg}"
            );
            assert!(
                msg.contains("query") || msg.contains("fragment"),
                "unexpected error message: {msg}"
            );
            assert!(
                !msg.contains("secret"),
                "SSO proxy URL query/fragment payload must not be echoed in config errors: {msg}"
            );
        } else {
            panic!("Expected BadConfig error about SSO proxy URL query/fragment");
        }
    }
}

#[tokio::test]
async fn test_validate_allowed_admin_origins_rejects_userinfo() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config
        .web
        .allowed_admin_origins
        .push("https://admin_user:secret@admin.example:7777".to_string());

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("[web].allowed_admin_origins"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("userinfo"), "unexpected error message: {msg}");
        assert!(
            !msg.contains("secret"),
            "allowed_admin_origins credentials must not be echoed in config errors: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about allowed_admin_origins userinfo");
    }
}

#[tokio::test]
async fn test_validate_allowed_admin_origins_rejects_path_query_fragment() {
    for origin in [
        "https://admin.example:7777/path",
        "https://admin.example:7777?token=secret",
        "https://admin.example:7777#secret",
    ] {
        let mut config = Config::default();
        use_strong_admin_password(&mut config);
        config.web.allowed_admin_origins.push(origin.to_string());

        let result = config.validate().await;

        if let Err(Error::BadConfig(msg)) = result {
            assert!(
                msg.contains("[web].allowed_admin_origins"),
                "unexpected error message: {msg}"
            );
            assert!(
                msg.contains("path") && msg.contains("query") && msg.contains("fragment"),
                "unexpected error message: {msg}"
            );
            assert!(
                !msg.contains("secret") && !msg.contains("token"),
                "allowed_admin_origins path/query/fragment payload must not be echoed: {msg}"
            );
        } else {
            panic!("Expected BadConfig error about allowed_admin_origins path/query/fragment");
        }
    }
}

fn config_with_single_pool_user() -> Config {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "app_user".to_string(),
        password: "secret".to_string(),
        pool_size: 1,
        ..User::default()
    });
    config.pools.insert("app_db".to_string(), pool);
    config
}

async fn assert_startup_identity_nul_rejected<F>(field: &str, mutate: F)
where
    F: FnOnce(&mut Config),
{
    let mut config = config_with_single_pool_user();
    mutate(&mut config);

    let err = config
        .validate()
        .await
        .expect_err("embedded NUL in backend startup identity must be rejected");
    match err {
        Error::BadConfig(msg) => {
            assert!(
                msg.contains(field),
                "error should identify {field}, got: {msg}"
            );
            assert!(msg.contains("NUL"), "unexpected error message: {msg}");
        }
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn test_validate_backend_startup_identity_rejects_embedded_nul() {
    assert_startup_identity_nul_rejected("server_database", |config| {
        config.pools.get_mut("app_db").unwrap().server_database =
            Some("postgres\0application_name\0evil".to_string());
    })
    .await;

    assert_startup_identity_nul_rejected("application_name", |config| {
        config.pools.get_mut("app_db").unwrap().application_name =
            Some("pg_doorman\0extra\0value".to_string());
    })
    .await;

    assert_startup_identity_nul_rejected("username", |config| {
        config.pools.get_mut("app_db").unwrap().users[0].username =
            "app_user\0database\0evil".to_string();
    })
    .await;

    assert_startup_identity_nul_rejected("server_username", |config| {
        config.pools.get_mut("app_db").unwrap().users[0].server_username =
            Some("backend_user\0database\0evil".to_string());
    })
    .await;

    assert_startup_identity_nul_rejected("pool name", |config| {
        let pool = config.pools.remove("app_db").unwrap();
        config
            .pools
            .insert("app_db\0application_name\0evil".to_string(), pool);
    })
    .await;
}

#[tokio::test]
async fn test_validate_auth_query_startup_identity_rejects_embedded_nul() {
    assert_startup_identity_nul_rejected("auth_query.user", |config| {
        let mut auth_query = valid_auth_query_config();
        auth_query.user = "auth_user\0database\0evil".to_string();
        config.pools.get_mut("app_db").unwrap().auth_query = Some(auth_query);
    })
    .await;

    assert_startup_identity_nul_rejected("auth_query.database", |config| {
        let mut auth_query = valid_auth_query_config();
        auth_query.database = Some("auth_db\0application_name\0evil".to_string());
        config.pools.get_mut("app_db").unwrap().auth_query = Some(auth_query);
    })
    .await;

    assert_startup_identity_nul_rejected("auth_query.server_user", |config| {
        let mut auth_query = valid_auth_query_config();
        auth_query.server_user = Some("shared_backend\0database\0evil".to_string());
        config.pools.get_mut("app_db").unwrap().auth_query = Some(auth_query);
    })
    .await;
}

#[tokio::test]
async fn test_validate_tls_rate_limit_less_than_100() {
    let mut config = Config::default();

    // Set invalid TLS rate limit
    config.general.tls_rate_limit_per_second = 50;

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("tls rate limit should be > 100"));
    } else {
        panic!("Expected BadConfig error about tls rate limit");
    }
}

// Test TLS rate limit not multiple of 100
#[tokio::test]
async fn test_validate_tls_rate_limit_not_multiple_of_100() {
    let mut config = Config::default();

    // Set invalid TLS rate limit
    config.general.tls_rate_limit_per_second = 150;

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("tls rate limit should be multiple 100"));
    } else {
        panic!("Expected BadConfig error about tls rate limit multiple");
    }
}

#[tokio::test]
async fn test_validate_tls_rate_limit_has_upper_bound() {
    let mut config = Config::default();
    config.general.tls_rate_limit_per_second = MAX_TLS_RATE_LIMIT_PER_SECOND + 100;

    let result = config.validate().await;

    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("tls rate limit must be <= "),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about tls rate limit upper bound");
    }
}

// Test HBA and pg_hba both set
#[tokio::test]
async fn test_validate_hba_and_pg_hba_both_set() {
    let mut config = Config::default();

    // Set both HBA settings
    config.general.hba = vec!["192.168.1.0/24".parse().unwrap()];
    config.general.pg_hba = Some(crate::auth::hba::PgHba::default());

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("general.hba and general.pg_hba cannot be specified at the same time"));
    } else {
        panic!("Expected BadConfig error about hba and pg_hba");
    }
}

#[tokio::test]
async fn test_validate_legacy_hba_with_unix_socket_rejected() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    config.general.unix_socket_dir = Some("/tmp".to_string());
    config.general.hba = vec!["10.0.0.0/8".parse().unwrap()];

    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("general.hba restricts TCP clients by CIDR"));
        assert!(msg.contains("general.pg_hba"));
    } else {
        panic!("Expected BadConfig error about legacy hba with unix_socket_dir");
    }
}

// Test prepared_statements enabled but cache_size is 0
#[tokio::test]
async fn test_validate_prepared_statements_no_cache() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Set invalid prepared statements config
    config.general.prepared_statements = true;
    config.general.prepared_statements_cache_size = 0;

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("prepared_statements_cache"));
    } else {
        panic!("Expected BadConfig error about prepared_statements_cache");
    }
}

#[tokio::test]
async fn test_validate_general_prepared_cache_upper_bound() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.prepared_statements = true;
    config.general.prepared_statements_cache_size = usize::MAX;

    let result = config.validate().await;
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.prepared_statements_cache_size must be <="),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about general prepared cache upper bound");
    }
}

#[tokio::test]
async fn test_validate_general_server_prepared_cache_upper_bound() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.prepared_statements = true;
    config.general.server_prepared_statements_cache_size = Some(usize::MAX);

    let result = config.validate().await;
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.server_prepared_statements_cache_size must be <="),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about general server prepared cache upper bound");
    }
}

#[tokio::test]
async fn test_validate_general_client_anonymous_prepared_cache_upper_bound() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.client_anonymous_prepared_cache_size = Some(usize::MAX);

    let result = config.validate().await;
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("general.client_anonymous_prepared_cache_size must be <="),
            "unexpected error message: {msg}"
        );
    } else {
        panic!(
            "Expected BadConfig error about general client anonymous prepared cache upper bound"
        );
    }
}

#[tokio::test]
async fn test_validate_pool_prepared_cache_upper_bound() {
    let mut pool = Pool {
        prepared_statements_cache_size: Some(usize::MAX),
        ..Pool::default()
    };

    let result = pool.validate().await;
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("pool.prepared_statements_cache_size must be <="),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about pool prepared cache upper bound");
    }
}

#[tokio::test]
async fn test_validate_pool_server_prepared_cache_upper_bound() {
    let mut pool = Pool {
        server_prepared_statements_cache_size: Some(usize::MAX),
        ..Pool::default()
    };

    let result = pool.validate().await;
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("pool.server_prepared_statements_cache_size must be <="),
            "unexpected error message: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about pool server prepared cache upper bound");
    }
}

// Test tls_certificate set but tls_private_key not set
#[tokio::test]
async fn test_validate_tls_certificate_without_private_key() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Set invalid TLS config
    config.general.tls_certificate = Some("cert.pem".to_string());
    config.general.tls_private_key = None;

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("tls_certificate is set but tls_private_key is not"));
    } else {
        panic!("Expected BadConfig error about tls_certificate without tls_private_key");
    }
}

// Test tls_private_key set but tls_certificate not set
#[tokio::test]
async fn test_validate_tls_private_key_without_certificate() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Set invalid TLS config
    config.general.tls_certificate = None;
    config.general.tls_private_key = Some("key.pem".to_string());

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("tls_private_key is set but tls_certificate is not"));
    } else {
        panic!("Expected BadConfig error about tls_private_key without tls_certificate");
    }
}

// Test tls_mode set but tls_certificate or tls_private_key not set
#[tokio::test]
async fn test_validate_tls_mode_without_cert_or_key() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Set invalid TLS config
    config.general.tls_mode = Some("require".to_string());
    config.general.tls_certificate = None;
    config.general.tls_private_key = None;

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("tls_mode is require but tls_certificate or tls_private_key is not"));
    } else {
        panic!("Expected BadConfig error about tls_mode without cert/key");
    }
}

// Test tls_mode is verify-full but tls_ca_cert is not set
#[tokio::test]
async fn test_validate_tls_mode_verify_full_without_ca_cert() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Set invalid TLS config
    config.general.tls_mode = Some("verify-full".to_string());
    config.general.tls_certificate = Some("cert.pem".to_string());
    config.general.tls_private_key = Some("key.pem".to_string());
    config.general.tls_ca_cert = None;

    // Validate should fail
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("tls_mode is verify-full but tls_ca_cert is not set"));
    } else {
        panic!("Expected BadConfig error about tls_mode verify-full without ca_cert");
    }
}

#[tokio::test]
async fn test_validate_tls_ca_cert_is_checked_before_startup() {
    let cert = NamedTempFile::new().unwrap();
    let key = NamedTempFile::new().unwrap();
    write_self_signed_tls_identity(cert.path(), key.path());

    let missing_ca = cert.path().with_extension("missing-ca.pem");
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.tls_certificate = Some(cert.path().display().to_string());
    config.general.tls_private_key = Some(key.path().display().to_string());
    config.general.tls_ca_cert = Some(missing_ca.display().to_string());
    config.general.tls_mode = Some("require".to_string());

    let result = config.validate().await;
    let err = result.expect_err("missing tls_ca_cert should fail config validation");
    let Error::BadConfig(msg) = err else {
        panic!("Expected BadConfig error about tls_ca_cert");
    };
    assert!(
        msg.contains("Failed to read certificate file") || msg.contains("tls_ca_cert"),
        "unexpected error message: {msg}"
    );
}

// Test valid TLS configuration with mode "allow"
#[tokio::test]
async fn test_validate_valid_tls_mode_allow() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Set valid TLS config for "allow" mode
    config.general.tls_mode = Some("allow".to_string());

    // For "allow" mode, certificates are optional
    // Test without certificates to avoid certificate validation
    let result = config.validate().await;
    assert!(
        result.is_ok(),
        "Validation should pass for 'allow' mode without certificates"
    );
}

// Test valid TLS configuration with mode "disable"
#[tokio::test]
async fn test_validate_valid_tls_mode_disable() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    // Set valid TLS config for "disable" mode
    config.general.tls_mode = Some("disable".to_string());

    // For "disable" mode, certificates are optional
    // Test without certificates to avoid certificate validation
    let result = config.validate().await;
    assert!(
        result.is_ok(),
        "Validation should pass for 'disable' mode without certificates"
    );
}

// ============================================================================
// Tests for YAML configuration support
// ============================================================================

#[test]
fn test_config_format_detect_toml() {
    assert_eq!(ConfigFormat::detect("config.toml"), ConfigFormat::Toml);
    assert_eq!(
        ConfigFormat::detect("/path/to/config.toml"),
        ConfigFormat::Toml
    );
    assert_eq!(ConfigFormat::detect("CONFIG.TOML"), ConfigFormat::Toml);
}

#[test]
fn test_config_format_detect_yaml() {
    assert_eq!(ConfigFormat::detect("config.yaml"), ConfigFormat::Yaml);
    assert_eq!(ConfigFormat::detect("config.yml"), ConfigFormat::Yaml);
    assert_eq!(
        ConfigFormat::detect("/path/to/config.yaml"),
        ConfigFormat::Yaml
    );
    assert_eq!(
        ConfigFormat::detect("/path/to/config.yml"),
        ConfigFormat::Yaml
    );
    assert_eq!(ConfigFormat::detect("CONFIG.YAML"), ConfigFormat::Yaml);
    assert_eq!(ConfigFormat::detect("CONFIG.YML"), ConfigFormat::Yaml);
}

#[test]
fn test_config_format_detect_default_to_toml() {
    // Unknown extensions should default to TOML
    assert_eq!(ConfigFormat::detect("config.json"), ConfigFormat::Toml);
    assert_eq!(ConfigFormat::detect("config"), ConfigFormat::Toml);
    assert_eq!(ConfigFormat::detect("config.txt"), ConfigFormat::Toml);
}

// Helper function to create a temporary YAML config file for testing
fn create_temp_yaml_config() -> NamedTempFile {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  example_db:
    server_host: "localhost"
    server_port: 5432
    idle_timeout: 40000
    users:
      - username: "example_user_1"
        password: "password1"
        pool_size: 40
        pool_mode: "transaction"
      - username: "example_user_2"
        password: "password2"
        pool_size: 20
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();
    temp_file
}

#[tokio::test]
#[serial]
async fn test_yaml_config_parsing() {
    let temp_file = create_temp_yaml_config();
    let file_path = temp_file.path().to_str().unwrap();

    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.pools.len(), 1);
    assert_eq!(config.pools["example_db"].idle_timeout, Some(40000));
    assert_eq!(
        config.pools["example_db"].users[0].username,
        "example_user_1"
    );
    assert_eq!(config.pools["example_db"].users[0].pool_size, 40);
    assert_eq!(
        config.pools["example_db"].users[0].pool_mode,
        Some(PoolMode::Transaction)
    );
    assert_eq!(
        config.pools["example_db"].users[1].username,
        "example_user_2"
    );
    assert_eq!(config.pools["example_db"].users[1].pool_size, 20);
}

#[tokio::test]
#[serial]
async fn test_yaml_config_serialize() {
    let temp_file = create_temp_yaml_config();
    let file_path = temp_file.path().to_str().unwrap();

    parse(file_path).await.unwrap();

    let config = get_config();
    // Test that config can be serialized to YAML
    let yaml_output = serde_yaml::to_string(&config).unwrap();
    assert!(yaml_output.contains("example_db"));
    assert!(yaml_output.contains("example_user_1"));

    // Test that config can be serialized to TOML
    let toml_output = toml::to_string_pretty(&config).unwrap();
    assert!(toml_output.contains("example_db"));
    assert!(toml_output.contains("example_user_1"));
}

#[test]
fn test_content_to_toml_string_toml() {
    let toml_content = r#"
[general]
host = "127.0.0.1"
port = 6432
"#;
    let result = content_to_toml_string(toml_content, ConfigFormat::Toml).unwrap();
    assert_eq!(result, toml_content);
}

#[test]
fn test_content_to_toml_string_yaml() {
    let yaml_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
"#;
    let result = content_to_toml_string(yaml_content, ConfigFormat::Yaml).unwrap();
    // Result should be valid TOML
    assert!(result.contains("[general]"));
    assert!(result.contains("host"));
    assert!(result.contains("port"));
}

#[test]
fn test_parse_config_content_toml() {
    let toml_content = r#"
[include]
files = []
"#;
    let result: GeneralWithInclude =
        parse_config_content(toml_content, ConfigFormat::Toml).unwrap();
    assert!(result.include.files.is_empty());
}

#[test]
fn test_parse_config_content_yaml() {
    let yaml_content = r#"
include:
  files: []
"#;
    let result: GeneralWithInclude =
        parse_config_content(yaml_content, ConfigFormat::Yaml).unwrap();
    assert!(result.include.files.is_empty());
}

// ============================================================================
// TOML Backward Compatibility Tests
// ============================================================================
// These tests verify that the old TOML format [pools.*.users.0] continues to work
// after the migration to the new array format [[pools.*.users]]

/// Test parsing legacy TOML format with [pools.*.users.0] syntax
#[tokio::test]
#[serial]
async fn test_toml_legacy_users_format() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[pools.example_db.users.0]
username = "legacy_user_1"
password = "password1"
pool_size = 30

[pools.example_db.users.1]
username = "legacy_user_2"
password = "password2"
pool_size = 20
"#;
    let mut temp_file = NamedTempFile::with_suffix(".toml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.pools.len(), 1);
    assert_eq!(config.pools["example_db"].users.len(), 2);
    assert_eq!(
        config.pools["example_db"].users[0].username,
        "legacy_user_1"
    );
    assert_eq!(config.pools["example_db"].users[0].pool_size, 30);
    assert_eq!(
        config.pools["example_db"].users[1].username,
        "legacy_user_2"
    );
    assert_eq!(config.pools["example_db"].users[1].pool_size, 20);
}

/// Test parsing new TOML format with [[pools.*.users]] syntax
#[tokio::test]
#[serial]
async fn test_toml_new_array_users_format() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "new_user_1"
password = "password1"
pool_size = 40

[[pools.example_db.users]]
username = "new_user_2"
password = "password2"
pool_size = 25
"#;
    let mut temp_file = NamedTempFile::with_suffix(".toml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.pools.len(), 1);
    assert_eq!(config.pools["example_db"].users.len(), 2);
    assert_eq!(config.pools["example_db"].users[0].username, "new_user_1");
    assert_eq!(config.pools["example_db"].users[0].pool_size, 40);
    assert_eq!(config.pools["example_db"].users[1].username, "new_user_2");
    assert_eq!(config.pools["example_db"].users[1].pool_size, 25);
}

/// Test parsing mixed TOML formats - different pools using different user formats
#[tokio::test]
#[serial]
async fn test_toml_mixed_users_formats() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.legacy_pool]
server_host = "localhost"
server_port = 5432

[pools.legacy_pool.users.0]
username = "legacy_user"
password = "password1"
pool_size = 30

[pools.new_pool]
server_host = "localhost"
server_port = 5433

[[pools.new_pool.users]]
username = "new_user"
password = "password2"
pool_size = 40
"#;
    let mut temp_file = NamedTempFile::with_suffix(".toml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.pools.len(), 2);

    // Check legacy pool
    assert_eq!(config.pools["legacy_pool"].users.len(), 1);
    assert_eq!(config.pools["legacy_pool"].users[0].username, "legacy_user");
    assert_eq!(config.pools["legacy_pool"].users[0].pool_size, 30);

    // Check new pool
    assert_eq!(config.pools["new_pool"].users.len(), 1);
    assert_eq!(config.pools["new_pool"].users[0].username, "new_user");
    assert_eq!(config.pools["new_pool"].users[0].pool_size, 40);
}

/// Test that legacy TOML format with multiple users preserves all user attributes
#[tokio::test]
#[serial]
async fn test_toml_legacy_format_all_user_attributes() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[pools.example_db.users.0]
username = "full_user"
password = "md5abcdef1234567890abcdef12345678"
pool_size = 50
min_pool_size = 5
pool_mode = "session"
server_lifetime = 3600000
server_username = "real_server_user"
server_password = "real_server_password"
"#;
    let mut temp_file = NamedTempFile::with_suffix(".toml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    let user = &config.pools["example_db"].users[0];

    assert_eq!(user.username, "full_user");
    assert_eq!(user.password, "md5abcdef1234567890abcdef12345678");
    assert_eq!(user.pool_size, 50);
    assert_eq!(user.min_pool_size, Some(5));
    assert_eq!(user.pool_mode, Some(PoolMode::Session));
    assert_eq!(user.server_lifetime, Some(3600000));
    assert_eq!(user.server_username, Some("real_server_user".to_string()));
    assert_eq!(
        user.server_password,
        Some("real_server_password".to_string())
    );
}

/// Test that duplicate usernames are rejected in legacy TOML format
#[tokio::test]
#[serial]
async fn test_toml_legacy_format_duplicate_username_rejected() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[pools.example_db.users.0]
username = "duplicate_user"
password = "password1"
pool_size = 30

[pools.example_db.users.1]
username = "duplicate_user"
password = "password2"
pool_size = 20
"#;
    let mut temp_file = NamedTempFile::with_suffix(".toml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    let result = parse(file_path).await;

    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("duplicate username"));
    } else {
        panic!("Expected BadConfig error about duplicate username");
    }
}

/// Test that duplicate usernames are rejected in new TOML array format
#[tokio::test]
#[serial]
async fn test_toml_new_format_duplicate_username_rejected() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "duplicate_user"
password = "password1"
pool_size = 30

[[pools.example_db.users]]
username = "duplicate_user"
password = "password2"
pool_size = 20
"#;
    let mut temp_file = NamedTempFile::with_suffix(".toml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    let result = parse(file_path).await;

    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("duplicate username"));
    } else {
        panic!("Expected BadConfig error about duplicate username");
    }
}

/// Test YAML format with array users (for comparison with TOML formats)
#[tokio::test]
#[serial]
async fn test_yaml_array_users_format() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  example_db:
    server_host: "localhost"
    server_port: 5432
    users:
      - username: "yaml_user_1"
        password: "password1"
        pool_size: 35
      - username: "yaml_user_2"
        password: "password2"
        pool_size: 15
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.pools.len(), 1);
    assert_eq!(config.pools["example_db"].users.len(), 2);
    assert_eq!(config.pools["example_db"].users[0].username, "yaml_user_1");
    assert_eq!(config.pools["example_db"].users[0].pool_size, 35);
    assert_eq!(config.pools["example_db"].users[1].username, "yaml_user_2");
    assert_eq!(config.pools["example_db"].users[1].pool_size, 15);
}

/// Test that duplicate usernames are rejected in YAML format
#[tokio::test]
#[serial]
async fn test_yaml_duplicate_username_rejected() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  example_db:
    server_host: "localhost"
    server_port: 5432
    users:
      - username: "duplicate_user"
        password: "password1"
        pool_size: 30
      - username: "duplicate_user"
        password: "password2"
        pool_size: 20
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    let result = parse(file_path).await;

    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("duplicate username"));
    } else {
        panic!("Expected BadConfig error about duplicate username");
    }
}

// ============================================================
// auth_query config tests
// ============================================================

fn valid_auth_query_config() -> pool::AuthQueryConfig {
    pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: "secret".to_string(),
        database: None,
        workers: 2,
        server_user: None,
        server_password: None,
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    }
}

/// Parse YAML config with auth_query in dedicated mode (server_user set)
#[tokio::test]
#[serial]
async fn test_auth_query_yaml_dedicated_mode() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  mydb:
    server_host: "localhost"
    server_port: 5432
    auth_query:
      query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1"
      user: "pg_doorman_auth"
      password: "secret"
      database: "postgres"
      workers: 3
      server_user: "backend_user"
      server_password: "backend_pass"
      pool_size: 50
      cache_ttl: "2h"
      cache_failure_ttl: "1m"
      min_interval: "2s"
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    let pool = &config.pools["mydb"];
    let aq = pool.auth_query.as_ref().unwrap();

    assert_eq!(
        aq.query,
        "SELECT usename, passwd FROM pg_shadow WHERE usename = $1"
    );
    assert_eq!(aq.user, "pg_doorman_auth");
    assert_eq!(aq.password, "secret");
    assert_eq!(aq.database, Some("postgres".to_string()));
    assert_eq!(aq.workers, 3);
    assert_eq!(aq.server_user, Some("backend_user".to_string()));
    assert_eq!(aq.server_password, Some("backend_pass".to_string()));
    assert_eq!(aq.pool_size, 50);
    assert_eq!(aq.cache_ttl, Duration::from_hours(2));
    assert_eq!(aq.cache_failure_ttl, Duration::from_mins(1));
    assert_eq!(aq.min_interval, Duration::from_secs(2));
    assert!(aq.is_dedicated_mode());
}

/// Parse YAML config with auth_query in passthrough mode (no server_user)
#[tokio::test]
#[serial]
async fn test_auth_query_yaml_passthrough_mode() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  mydb:
    server_host: "localhost"
    server_port: 5432
    auth_query:
      query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1"
      user: "pg_doorman_auth"
      password: "secret"
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    let pool = &config.pools["mydb"];
    let aq = pool.auth_query.as_ref().unwrap();

    assert_eq!(aq.user, "pg_doorman_auth");
    assert_eq!(aq.server_user, None);
    assert_eq!(aq.server_password, None);
    assert!(!aq.is_dedicated_mode());
    // Verify defaults
    assert_eq!(aq.workers, 2);
    assert_eq!(aq.pool_size, 40);
    assert_eq!(aq.cache_ttl, Duration::from_hours(1));
    assert_eq!(aq.cache_failure_ttl, Duration::from_secs(30));
    assert_eq!(aq.min_interval, Duration::from_secs(1));
}

/// Parse YAML config without auth_query (backward compatibility)
#[tokio::test]
#[serial]
async fn test_auth_query_yaml_absent() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  mydb:
    server_host: "localhost"
    server_port: 5432
    users:
      - username: "user1"
        password: "pass1"
        pool_size: 10
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert!(config.pools["mydb"].auth_query.is_none());
}

/// Parse TOML config with auth_query section
#[tokio::test]
#[serial]
async fn test_auth_query_toml() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.mydb]
server_host = "localhost"
server_port = 5432

[pools.mydb.auth_query]
query = "SELECT usename, passwd FROM pg_shadow WHERE usename = $1"
user = "pg_doorman_auth"
password = "secret"
workers = 2
pool_size = 40
cache_ttl = 3600000
cache_failure_ttl = 30000
min_interval = 1000

[[pools.mydb.users]]
username = "static_user"
password = "static_pass"
pool_size = 10
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    let pool = &config.pools["mydb"];
    let aq = pool.auth_query.as_ref().unwrap();

    assert_eq!(
        aq.query,
        "SELECT usename, passwd FROM pg_shadow WHERE usename = $1"
    );
    assert_eq!(aq.user, "pg_doorman_auth");
    assert_eq!(aq.cache_ttl, Duration::from_millis(3600000));
    // Static users still work alongside auth_query
    assert_eq!(pool.users.len(), 1);
    assert_eq!(pool.users[0].username, "static_user");
}

/// Validation: empty query produces error
#[tokio::test]
async fn test_auth_query_validate_empty_query() {
    let mut pool = Pool::default();
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: "secret".to_string(),
        database: None,
        workers: 2,
        server_user: None,
        server_password: None,
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("auth_query.query cannot be empty"));
    }
}

#[tokio::test]
async fn test_auth_query_rejects_global_required_server_tls_mode() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.server_tls_mode = "require".to_string();

    let mut pool = Pool::default();
    pool.auth_query = Some(valid_auth_query_config());
    config.pools.insert("mydb".to_string(), pool);

    let result = config.validate().await;

    let err = result.expect_err("auth_query must reject required server_tls_mode");
    let msg = format!("{err}");
    assert!(
        msg.contains("auth_query") && msg.contains("server_tls_mode") && msg.contains("require"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn test_auth_query_rejects_pool_required_server_tls_override() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);

    let mut pool = Pool {
        server_tls_mode: Some("require".to_string()),
        ..Pool::default()
    };
    pool.auth_query = Some(valid_auth_query_config());
    config.pools.insert("mydb".to_string(), pool);

    let result = config.validate().await;

    let err = result.expect_err("auth_query must reject pool server_tls_mode override");
    let msg = format!("{err}");
    assert!(
        msg.contains("auth_query") && msg.contains("server_tls_mode") && msg.contains("require"),
        "unexpected error: {msg}"
    );
}

/// Validation: empty user produces error
#[tokio::test]
async fn test_auth_query_validate_empty_user() {
    let mut pool = Pool::default();
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "".to_string(),
        password: "secret".to_string(),
        database: None,
        workers: 2,
        server_user: None,
        server_password: None,
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("auth_query.user cannot be empty"));
    }
}

/// Validation: server_password without server_user produces error
#[tokio::test]
async fn test_auth_query_validate_server_password_without_server_user() {
    let mut pool = Pool::default();
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: "secret".to_string(),
        database: None,
        workers: 2,
        server_user: None,
        server_password: Some("orphan_password".to_string()),
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("server_password requires server_user"));
    }
}

/// Validation: server_user without server_password is valid (trust auth)
#[tokio::test]
async fn test_auth_query_validate_server_user_without_password_ok() {
    let mut pool = Pool::default();
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: String::new(),
        database: None,
        workers: 2,
        server_user: Some("backend_user".to_string()),
        server_password: None,
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_auth_query_validate_server_user_static_user_collision() {
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "backend_user".to_string(),
        password: "static_password".to_string(),
        server_username: Some("different_backend_role".to_string()),
        pool_size: 1,
        ..User::default()
    });
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: String::new(),
        database: None,
        workers: 2,
        server_user: Some("backend_user".to_string()),
        server_password: None,
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("auth_query.server_user")
                && msg.contains("backend_user")
                && msg.contains("pool user"),
            "unexpected message: {msg}"
        );
    }
}

/// Validation: pool_size 0 produces error
#[tokio::test]
async fn test_auth_query_validate_pool_size_zero() {
    let mut pool = Pool::default();
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: "secret".to_string(),
        database: None,
        workers: 0,
        server_user: None,
        server_password: None,
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("auth_query.workers must be > 0"));
    }
}

#[tokio::test]
async fn test_auth_query_validate_workers_upper_bound() {
    let mut pool = Pool::default();
    pool.auth_query = Some(valid_auth_query_config());
    pool.auth_query.as_mut().unwrap().workers = u32::MAX;

    let result = pool.validate().await;
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("auth_query.workers must be <="),
            "unexpected error: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about auth_query.workers upper bound");
    }
}

/// Validation: empty password is valid (PostgreSQL trust auth for executor)
#[tokio::test]
async fn test_auth_query_validate_empty_password_ok() {
    let mut pool = Pool::default();
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: String::new(),
        database: None,
        workers: 2,
        server_user: None,
        server_password: None,
        pool_size: 40,
        min_pool_size: 0,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_ok());
}

/// Validation: min_pool_size > pool_size must be rejected
#[tokio::test]
async fn test_auth_query_validate_min_pool_size_exceeds_pool_size() {
    let mut pool = Pool::default();
    pool.auth_query = Some(pool::AuthQueryConfig {
        query: "SELECT usename, passwd FROM pg_shadow WHERE usename = $1".to_string(),
        user: "pg_doorman_auth".to_string(),
        password: "secret".to_string(),
        database: None,
        workers: 2,
        server_user: None,
        server_password: None,
        pool_size: 5,
        min_pool_size: 10,
        cache_ttl: Duration::from_hours(1),
        cache_failure_ttl: Duration::from_secs(30),
        min_interval: Duration::from_secs(1),
    });

    let result = pool.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("min_pool_size must be <= pool_size"),
            "unexpected error: {msg}"
        );
    }
}

#[tokio::test]
async fn test_auth_query_validate_pool_size_upper_bound() {
    let mut pool = Pool::default();
    pool.auth_query = Some(valid_auth_query_config());
    pool.auth_query.as_mut().unwrap().pool_size = u32::MAX;

    let result = pool.validate().await;
    if let Err(Error::BadConfig(msg)) = result {
        assert!(
            msg.contains("auth_query.pool_size must be <="),
            "unexpected error: {msg}"
        );
    } else {
        panic!("Expected BadConfig error about auth_query.pool_size upper bound");
    }
}

// ============================================================
// Scaling config tests
// ============================================================

/// Test 1: Parsing YAML with general scaling fields
#[tokio::test]
#[serial]
async fn test_scaling_config_general_yaml() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"
  scaling_warm_pool_ratio: 30
  scaling_fast_retries: 20
  scaling_max_parallel_creates: 4

pools:
  mydb:
    server_host: "localhost"
    server_port: 5432
    users:
      - username: "user1"
        password: "pass1"
        pool_size: 10
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.general.scaling_warm_pool_ratio, 30);
    assert_eq!(config.general.scaling_fast_retries, 20);
    assert_eq!(config.general.scaling_max_parallel_creates, 4);
}

/// Test 2: Parsing defaults when scaling fields omitted
#[tokio::test]
#[serial]
async fn test_scaling_config_defaults() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  mydb:
    server_host: "localhost"
    server_port: 5432
    users:
      - username: "user1"
        password: "pass1"
        pool_size: 10
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.general.scaling_warm_pool_ratio, 20);
    assert_eq!(config.general.scaling_fast_retries, 10);
    assert_eq!(config.general.scaling_max_parallel_creates, 2);
    // Pool-level should be None
    let pool = &config.pools["mydb"];
    assert_eq!(pool.scaling_warm_pool_ratio, None);
    assert_eq!(pool.scaling_fast_retries, None);
}

/// Test 3: Pool-level override parsing
#[tokio::test]
#[serial]
async fn test_scaling_config_pool_override_yaml() {
    let config_content = r#"
general:
  host: "127.0.0.1"
  port: 6432
  admin_username: "admin"
  admin_password: "admin_password"

pools:
  overridden_db:
    server_host: "localhost"
    server_port: 5432
    scaling_warm_pool_ratio: 50
    scaling_fast_retries: 5
    users:
      - username: "user1"
        password: "pass1"
        pool_size: 10
  default_db:
    server_host: "localhost"
    server_port: 5432
    users:
      - username: "user2"
        password: "pass2"
        pool_size: 10
"#;
    let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    let overridden = &config.pools["overridden_db"];
    assert_eq!(overridden.scaling_warm_pool_ratio, Some(50));
    assert_eq!(overridden.scaling_fast_retries, Some(5));

    let default = &config.pools["default_db"];
    assert_eq!(default.scaling_warm_pool_ratio, None);
    assert_eq!(default.scaling_fast_retries, None);
}

/// Test 4: resolve_scaling_config() - pool override wins
#[tokio::test]
async fn test_resolve_scaling_config_pool_override() {
    let mut general = General::default();
    general.scaling_warm_pool_ratio = 20;
    general.scaling_fast_retries = 10;
    general.scaling_max_parallel_creates = 2;

    let pool = Pool {
        scaling_warm_pool_ratio: Some(50),
        ..Pool::default()
    };

    let scaling = pool.resolve_scaling_config(&general);
    assert!((scaling.warm_pool_ratio - 0.5).abs() < f32::EPSILON);
    assert_eq!(scaling.fast_retries, 10); // general default
    assert_eq!(scaling.max_parallel_creates, 2); // global only
}

/// Test 5: resolve_scaling_config() - general fallback
#[tokio::test]
async fn test_resolve_scaling_config_general_fallback() {
    let mut general = General::default();
    general.scaling_warm_pool_ratio = 30;
    general.scaling_fast_retries = 15;
    general.scaling_max_parallel_creates = 3;

    let pool = Pool::default(); // all scaling fields are None

    let scaling = pool.resolve_scaling_config(&general);
    assert!((scaling.warm_pool_ratio - 0.3).abs() < f32::EPSILON);
    assert_eq!(scaling.fast_retries, 15);
    assert_eq!(scaling.max_parallel_creates, 3);
}

/// Test 5b: Validation rejects max_parallel_creates = 0 (would deadlock create path)
#[tokio::test]
async fn test_validate_scaling_max_parallel_creates_zero_rejected() {
    let mut config = Config::default();
    config.general.scaling_max_parallel_creates = 0;
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("scaling_max_parallel_creates"));
    } else {
        panic!("Expected BadConfig error about scaling_max_parallel_creates");
    }
}

/// Test 6: Validation - general warm_pool_ratio > 100
#[tokio::test]
async fn test_validate_scaling_warm_pool_ratio_general_out_of_range() {
    let mut config = Config::default();
    config.general.scaling_warm_pool_ratio = 150;
    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("scaling_warm_pool_ratio"));
    } else {
        panic!("Expected BadConfig error about scaling_warm_pool_ratio");
    }
}

/// Test 7: Validation - pool warm_pool_ratio > 100
#[tokio::test]
async fn test_validate_scaling_warm_pool_ratio_pool_out_of_range() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    let pool = Pool {
        scaling_warm_pool_ratio: Some(101),
        users: vec![User {
            username: "user1".to_string(),
            password: "pass1".to_string(),
            ..User::default()
        }],
        ..Pool::default()
    };
    config.pools.insert("testdb".to_string(), pool);

    let result = config.validate().await;
    assert!(result.is_err());
    if let Err(Error::BadConfig(msg)) = result {
        assert!(msg.contains("scaling_warm_pool_ratio"));
    } else {
        panic!("Expected BadConfig error about scaling_warm_pool_ratio");
    }
}

/// Test 8: Hash changes when scaling config changes
#[test]
fn test_scaling_config_changes_pool_hash() {
    let pool_a = Pool {
        scaling_warm_pool_ratio: None,
        ..Pool::default()
    };
    let pool_b = Pool {
        scaling_warm_pool_ratio: Some(50),
        ..Pool::default()
    };
    assert_ne!(pool_a.hash_value(), pool_b.hash_value());
}

#[tokio::test]
#[serial]
async fn test_scaling_config_toml_parsing() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"
scaling_warm_pool_ratio = 40
scaling_fast_retries = 15
scaling_max_parallel_creates = 3

[pools.mydb]
server_host = "localhost"
server_port = 5432
scaling_warm_pool_ratio = 60

[[pools.mydb.users]]
username = "user1"
password = "pass1"
pool_size = 10
"#;
    let mut temp_file = NamedTempFile::with_suffix(".toml").unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let file_path = temp_file.path().to_str().unwrap();
    parse(file_path).await.unwrap();

    let config = get_config();
    assert_eq!(config.general.scaling_warm_pool_ratio, 40);
    assert_eq!(config.general.scaling_fast_retries, 15);
    assert_eq!(config.general.scaling_max_parallel_creates, 3);

    let pool = &config.pools["mydb"];
    assert_eq!(pool.scaling_warm_pool_ratio, Some(60));
    assert_eq!(pool.scaling_fast_retries, None);
}

/// Test 10: Edge case - warm_pool_ratio = 0 and 100
#[tokio::test]
async fn test_scaling_config_boundary_values() {
    let general = General::default();

    // warm_pool_ratio = 0 -> valid, all connections go through cooldown
    let pool_zero = Pool {
        scaling_warm_pool_ratio: Some(0),
        ..Pool::default()
    };
    let mut pool_zero_for_validate = pool_zero.clone();
    assert!(pool_zero_for_validate.validate().await.is_ok());
    let scaling = pool_zero.resolve_scaling_config(&general);
    assert!((scaling.warm_pool_ratio - 0.0).abs() < f32::EPSILON);

    // warm_pool_ratio = 100 -> valid, all connections created immediately
    let pool_hundred = Pool {
        scaling_warm_pool_ratio: Some(100),
        ..Pool::default()
    };
    let mut pool_hundred_for_validate = pool_hundred.clone();
    assert!(pool_hundred_for_validate.validate().await.is_ok());
    let scaling = pool_hundred.resolve_scaling_config(&general);
    assert!((scaling.warm_pool_ratio - 1.0).abs() < f32::EPSILON);
}

// --- Pool coordinator validation tests ---
// These validations produce warnings (log::warn), not errors.
// We verify that the config is accepted (Ok) despite the suboptimal settings.

#[tokio::test]
async fn test_validate_coordinator_sum_min_pool_size_exceeds_max() {
    let mut pool = Pool {
        max_db_connections: Some(10),
        users: vec![
            User {
                username: "u1".to_string(),
                password: "p1".to_string(),
                min_pool_size: Some(6),
                pool_size: 10,
                ..Default::default()
            },
            User {
                username: "u2".to_string(),
                password: "p2".to_string(),
                min_pool_size: Some(6),
                pool_size: 10,
                ..Default::default()
            },
        ],
        ..Pool::default()
    };
    // sum(min_pool_size) = 12 > max_db_connections = 10 -> rejected
    let err = pool.validate().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("sum of min_pool_size"),
        "error should mention min_pool_size sum: {msg}"
    );
}

#[tokio::test]
async fn test_validate_coordinator_user_pool_size_exceeds_max() {
    let mut pool = Pool {
        max_db_connections: Some(5),
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 20,
            ..Default::default()
        }],
        ..Pool::default()
    };
    // user.pool_size = 20 > max_db_connections = 5 -> accepted with warning
    assert!(pool.validate().await.is_ok());
}

#[tokio::test]
async fn test_validate_coordinator_min_lifetime_exceeds_idle_timeout() {
    let mut pool = Pool {
        max_db_connections: Some(10),
        min_connection_lifetime: Some(30000),
        idle_timeout: Some(5000),
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 10,
            ..Default::default()
        }],
        ..Pool::default()
    };
    // min_connection_lifetime(30s) > idle_timeout(5s) -> accepted with warning
    assert!(pool.validate().await.is_ok());
}

#[tokio::test]
async fn test_validate_coordinator_guaranteed_exceeds_pool_size() {
    let mut pool = Pool {
        max_db_connections: Some(10),
        min_guaranteed_pool_size: Some(8),
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 5,
            ..Default::default()
        }],
        ..Pool::default()
    };
    // min_guaranteed_pool_size(8) > pool_size(5) -> accepted with warning
    assert!(pool.validate().await.is_ok());
}

#[tokio::test]
async fn test_validate_coordinator_disabled_skips_all_checks() {
    let mut pool = Pool {
        max_db_connections: Some(0), // disabled
        min_guaranteed_pool_size: Some(100),
        min_connection_lifetime: Some(999999),
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 50,
            min_pool_size: Some(50),
            ..Default::default()
        }],
        ..Pool::default()
    };
    // max_db_connections=0 -> coordinator disabled, no warnings checked
    assert!(pool.validate().await.is_ok());
}

#[tokio::test]
async fn test_validate_coordinator_reserve_exceeds_max_db_connections_accepted_with_warning() {
    let mut pool = Pool {
        max_db_connections: Some(5),
        reserve_pool_size: Some(10), // 10 > 5 -> warn but OK
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 5,
            ..Default::default()
        }],
        ..Pool::default()
    };
    // reserve_pool_size > max_db_connections -> warning only, not error
    assert!(pool.validate().await.is_ok());
}

#[tokio::test]
async fn test_validate_reserve_pool_timeout_exceeds_query_wait_timeout_accepted_with_warning() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.query_wait_timeout = Duration::from_millis(2000);

    let mut pool = Pool {
        max_db_connections: Some(10),
        reserve_pool_timeout: Some(5000), // 5000 > 2000 -> warn but OK
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 10,
            ..Default::default()
        }],
        ..Pool::default()
    };
    pool.validate().await.unwrap();
    config.pools.insert("test_db".to_string(), pool);

    // Cross-config validation: reserve_pool_timeout > query_wait_timeout -> accepted with warning
    assert!(config.validate().await.is_ok());
}

#[tokio::test]
async fn test_validate_reserve_pool_timeout_within_query_wait_timeout_no_warning() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.query_wait_timeout = Duration::from_millis(5000);

    let mut pool = Pool {
        max_db_connections: Some(10),
        reserve_pool_timeout: Some(3000), // 3000 < 5000 -> no warning
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 10,
            ..Default::default()
        }],
        ..Pool::default()
    };
    pool.validate().await.unwrap();
    config.pools.insert("test_db".to_string(), pool);

    assert!(config.validate().await.is_ok());
}

#[tokio::test]
async fn test_validate_reserve_pool_timeout_skipped_when_coordinator_disabled() {
    let mut config = Config::default();
    use_strong_admin_password(&mut config);
    config.general.query_wait_timeout = Duration::from_millis(1000);

    let mut pool = Pool {
        max_db_connections: Some(0), // coordinator disabled
        reserve_pool_timeout: Some(9999),
        users: vec![User {
            username: "u1".to_string(),
            password: "p1".to_string(),
            pool_size: 10,
            ..Default::default()
        }],
        ..Pool::default()
    };
    pool.validate().await.unwrap();
    config.pools.insert("test_db".to_string(), pool);

    // Coordinator disabled -> no cross-config check
    assert!(config.validate().await.is_ok());
}

// ---- check_hba_with_general: legacy general.hba + Unix socket semantics ----

fn tcp_transport(ip: &str) -> ClientTransport {
    let peer = std::net::SocketAddr::new(ip.parse().unwrap(), 12345);
    ClientTransport::Tcp { peer, ssl: false }
}

#[test]
fn check_hba_legacy_empty_allows_unix() {
    // Legacy branch, nothing configured: any transport is allowed. Kept as a
    // baseline so the next two tests document what changes once Unix
    // enters the picture.
    let general = General::default();
    assert_eq!(
        check_hba_with_general(&general, &ClientTransport::Unix, "md5", "alice", "app"),
        CheckResult::Allow
    );
    assert_eq!(
        check_hba_with_general(&general, &tcp_transport("10.0.0.5"), "md5", "alice", "app"),
        CheckResult::Allow
    );
}

#[test]
fn check_hba_legacy_list_bypassed_for_unix() {
    // The legacy CIDR allowlist applies only to TCP clients. Unix socket
    // clients must still be allowed because the legacy list has no
    // transport concept.
    let mut general = General::default();
    general.hba = vec!["10.0.0.0/8".parse().unwrap()];

    // Unix: Allow regardless of source IP
    assert_eq!(
        check_hba_with_general(&general, &ClientTransport::Unix, "md5", "alice", "app"),
        CheckResult::Allow
    );
    // TCP from an IP outside the whitelist: NotMatched
    assert_eq!(
        check_hba_with_general(
            &general,
            &tcp_transport("192.168.1.10"),
            "md5",
            "alice",
            "app"
        ),
        CheckResult::NotMatched
    );
    // TCP from an IP inside the whitelist: Allow
    assert_eq!(
        check_hba_with_general(&general, &tcp_transport("10.1.2.3"), "md5", "alice", "app"),
        CheckResult::Allow
    );
}

#[test]
fn check_hba_pg_hba_takes_precedence_over_legacy_for_unix() {
    // When pg_hba is configured the legacy list must be ignored entirely;
    // `local` rules drive the decision for Unix clients.
    use crate::auth::hba::PgHba;
    let mut general = General::default();
    general.hba = vec!["10.0.0.0/8".parse().unwrap()];
    general.pg_hba = Some(PgHba::from_content("local all all reject"));

    assert_eq!(
        check_hba_with_general(&general, &ClientTransport::Unix, "md5", "alice", "app"),
        CheckResult::Deny
    );
}

// ---- legacy_hba_bypassed_by_unix_socket: silent privilege expansion detector ----

#[test]
fn legacy_hba_bypass_detected_when_unix_dir_set_and_legacy_hba_present() {
    let mut general = General::default();
    general.unix_socket_dir = Some("/tmp".to_string());
    general.hba = vec!["10.0.0.0/8".parse().unwrap()];
    assert!(legacy_hba_bypassed_by_unix_socket(&general));
}

#[test]
fn legacy_hba_bypass_quiet_without_unix_socket_dir() {
    let mut general = General::default();
    general.hba = vec!["10.0.0.0/8".parse().unwrap()];
    // No unix listener -> operator's CIDR whitelist applies to every client.
    assert!(!legacy_hba_bypassed_by_unix_socket(&general));
}

#[test]
fn legacy_hba_bypass_quiet_without_legacy_entries() {
    let mut general = General::default();
    general.unix_socket_dir = Some("/tmp".to_string());
    // Empty legacy hba means there is no rule to bypass in the first place.
    assert!(!legacy_hba_bypassed_by_unix_socket(&general));
}

#[test]
fn legacy_hba_bypass_quiet_when_pg_hba_present() {
    use crate::auth::hba::PgHba;
    let mut general = General::default();
    general.unix_socket_dir = Some("/tmp".to_string());
    general.hba = vec!["10.0.0.0/8".parse().unwrap()];
    general.pg_hba = Some(PgHba::from_content("local all all trust"));
    // pg_hba takes precedence and has explicit local rules - no silent bypass.
    assert!(!legacy_hba_bypassed_by_unix_socket(&general));
}

#[test]
fn deprecated_general_keys_yaml_detects_old_field_under_general() {
    let yaml = r#"
general:
  host: "0.0.0.0"
  client_prepared_statements_cache_size: 1024
"#;
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
    let found = find_deprecated_general_keys_yaml(&value);
    assert_eq!(found, vec!["client_prepared_statements_cache_size"]);
}

#[test]
fn deprecated_general_keys_yaml_detects_old_field_at_root() {
    // YAML configs sometimes place `general` keys at the document root
    // (used in tests like `old_field_is_aliased_to_new_field`).
    let yaml = r#"
host: "0.0.0.0"
client_prepared_statements_cache_size: 1024
"#;
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
    let found = find_deprecated_general_keys_yaml(&value);
    assert_eq!(found, vec!["client_prepared_statements_cache_size"]);
}

#[test]
fn deprecated_general_keys_yaml_returns_empty_when_absent() {
    let yaml = r#"
general:
  host: "0.0.0.0"
  client_anonymous_prepared_cache_size: 1024
"#;
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
    let found = find_deprecated_general_keys_yaml(&value);
    assert!(found.is_empty());
}

#[test]
fn deprecated_general_keys_toml_detects_old_field() {
    let toml_input = r#"
[general]
host = "0.0.0.0"
client_prepared_statements_cache_size = 2048
"#;
    let value: toml::Value = toml_input.parse().unwrap();
    let found = find_deprecated_general_keys_toml(&value);
    assert_eq!(found, vec!["client_prepared_statements_cache_size"]);
}

#[test]
fn deprecated_general_keys_toml_returns_empty_when_absent() {
    let toml_input = r#"
[general]
host = "0.0.0.0"
client_anonymous_prepared_cache_size = 2048
"#;
    let value: toml::Value = toml_input.parse().unwrap();
    let found = find_deprecated_general_keys_toml(&value);
    assert!(found.is_empty());
}

#[tokio::test]
#[serial]
async fn test_config_web_section() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[web]
enabled = true
host = "127.0.0.1"
port = 9128
ui = true
ui_anonymous = false
log_tap_max_entries = 4096

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    parse(temp_file.path().to_str().unwrap()).await.unwrap();

    let cfg = get_config();
    assert!(cfg.web.enabled);
    assert_eq!(cfg.web.host, "127.0.0.1");
    assert_eq!(cfg.web.port, 9128);
    assert!(cfg.web.ui);
    assert!(!cfg.web.ui_anonymous);
    assert_eq!(cfg.web.log_tap_max_entries, 4096);
}

#[tokio::test]
#[serial]
async fn test_config_prometheus_alias() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[prometheus]
enabled = true
host = "127.0.0.1"
port = 9128

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    parse(temp_file.path().to_str().unwrap()).await.unwrap();

    let cfg = get_config();
    assert!(cfg.web.enabled);
    assert_eq!(cfg.web.host, "127.0.0.1");
    assert_eq!(cfg.web.port, 9128);
    // New-field defaults are preserved when the legacy [prometheus] alias is used.
    assert!(!cfg.web.ui);
    assert!(!cfg.web.ui_anonymous);
    assert_eq!(cfg.web.log_tap_max_entries, 8192);
}

#[tokio::test]
#[serial]
async fn test_config_web_and_prometheus_both_rejected() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[web]
enabled = true

[prometheus]
enabled = false

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let result = parse(temp_file.path().to_str().unwrap()).await;
    assert!(
        result.is_err(),
        "Expected parse to fail when both [web] and [prometheus] are present, but it succeeded"
    );
}

#[tokio::test]
#[serial]
async fn test_config_web_section_partial() {
    let config_content = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[web]
enabled = true

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(config_content.as_bytes()).unwrap();
    temp_file.flush().unwrap();

    parse(temp_file.path().to_str().unwrap()).await.unwrap();

    let cfg = get_config();
    assert!(cfg.web.enabled);
    // All other fields fall back to defaults.
    assert_eq!(cfg.web.host, "0.0.0.0");
    assert_eq!(cfg.web.port, 9127);
    assert!(!cfg.web.ui);
    assert!(!cfg.web.ui_anonymous);
    assert_eq!(cfg.web.log_tap_max_entries, 8192);
}

#[tokio::test]
#[serial]
async fn reload_pool_apply_error_does_not_publish_config() {
    let initial_config = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let reloaded_config = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "new_admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432
server_tls_mode = "verify-ca"
server_tls_ca_cert = "/definitely/missing/pg_doorman_reload_ca.pem"

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(initial_config.as_bytes()).unwrap();
    temp_file.flush().unwrap();
    let path = temp_file.path().to_str().unwrap();

    parse(path).await.unwrap();
    assert_eq!(get_config().general.port, 6432);
    assert_eq!(get_config().general.admin_username, "admin");

    std::fs::write(path, reloaded_config).unwrap();

    let csm: ClientServerMap = std::sync::Arc::new(crate::utils::dashmap::new_dashmap(1));
    let err = reload_config(csm)
        .await
        .expect_err("missing pool server_tls_ca_cert should reject runtime reload");
    assert!(
        err.to_string().contains("pg_doorman_reload_ca.pem"),
        "unexpected reload error: {err}"
    );
    assert_eq!(
        get_config().general.admin_username,
        "admin",
        "failed pool apply must not publish the parsed reload config"
    );
}

#[tokio::test]
#[serial]
async fn reload_rejects_restart_only_listener_changes_without_publishing() {
    let initial_config = r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let reloaded_config = r#"
[general]
host = "127.0.0.1"
port = 7432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#;
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(initial_config.as_bytes()).unwrap();
    temp_file.flush().unwrap();
    let path = temp_file.path().to_str().unwrap();

    parse(path).await.unwrap();
    assert_eq!(get_config().general.port, 6432);

    std::fs::write(path, reloaded_config).unwrap();

    let csm: ClientServerMap = std::sync::Arc::new(crate::utils::dashmap::new_dashmap(1));
    let err = reload_config(csm)
        .await
        .expect_err("listener bind fields must require a process restart");
    let msg = err.to_string();
    assert!(
        msg.contains("general.port") && msg.contains("restart"),
        "unexpected reload error: {msg}"
    );
    assert_eq!(
        get_config().general.port,
        6432,
        "rejected listener reload must not publish unapplied listener config"
    );
}

#[test]
#[serial]
fn apply_general_log_level_updates_live_filter() {
    ensure_test_log_controller();
    crate::app::log_level::set_log_level("info").unwrap();

    let mut config = Config::default();
    config.general.log_level = Some("debug".to_string());

    super::apply_general_log_level(&config);
    assert_eq!(
        crate::app::log_level::get_log_level(),
        "debug",
        "successful config reload must apply [general].log_level to the live logger"
    );

    crate::app::log_level::set_log_level("info").unwrap();
}

#[tokio::test]
#[serial]
async fn failed_reload_does_not_publish_talos_keys() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("pg_doorman.toml");
    let old_key = dir.path().join("old_talos.pem");
    let new_key = dir.path().join("new_talos.pem");
    write_rsa_public_key(&old_key);
    write_rsa_public_key(&new_key);
    let old_key = old_key.to_str().unwrap();
    let new_key = new_key.to_str().unwrap();
    let config_path_str = config_path.to_str().unwrap();

    let initial_config = format!(
        r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[talos]
keys = ["{old_key}"]
databases = ["example_db"]
resource_prefixes = ["postgres.stg"]

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#
    );
    std::fs::write(&config_path, initial_config).unwrap();
    {
        let mut keys = crate::auth::talos::TALOS_KEYS.write().await;
        keys.clear();
    }

    parse(config_path_str).await.unwrap();
    {
        let keys = crate::auth::talos::TALOS_KEYS.read().await;
        assert!(keys.contains_key("old_talos"));
        assert!(!keys.contains_key("new_talos"));
    }

    let reloaded_config = format!(
        r#"
[general]
host = "127.0.0.1"
port = 7432
admin_username = "admin"
admin_password = "admin_password"

[talos]
keys = ["{new_key}"]
databases = ["example_db"]
resource_prefixes = ["postgres.stg"]

[pools.example_db]
server_host = "localhost"
server_port = 5432
server_tls_mode = "verify-ca"
server_tls_ca_cert = "/definitely/missing/pg_doorman_reload_ca.pem"

[[pools.example_db.users]]
username = "u"
password = "p"
pool_size = 5
"#
    );
    std::fs::write(&config_path, reloaded_config).unwrap();

    let csm: ClientServerMap = std::sync::Arc::new(crate::utils::dashmap::new_dashmap(1));
    reload_config(csm)
        .await
        .expect_err("missing pool server_tls_ca_cert should reject runtime reload");

    let keys = crate::auth::talos::TALOS_KEYS.read().await;
    assert!(
        keys.contains_key("old_talos"),
        "failed reload must keep old Talos trust roots"
    );
    assert!(
        !keys.contains_key("new_talos"),
        "failed reload must not publish uncommitted Talos trust roots"
    );
}

#[tokio::test]
#[serial]
async fn validate_jwt_password_requires_issuer_and_audience_scope() {
    let dir = tempfile::tempdir().unwrap();
    let public_key = dir.path().join("jwt_public.pem");
    write_rsa_public_key(&public_key);
    let public_key_str = public_key.to_str().unwrap();
    let user = User {
        username: "jwt_user".to_string(),
        password: format!("jwt-pkey-fpath:{public_key_str}"),
        pool_size: 1,
        ..User::default()
    };

    let err = user
        .validate()
        .await
        .expect_err("JWT config must require issuer/audience scoping");

    assert!(
        err.to_string().contains("JWT")
            && err.to_string().contains("issuer")
            && err.to_string().contains("audience"),
        "unexpected JWT scope validation error: {err}"
    );
}

#[tokio::test]
#[serial]
async fn unchanged_reload_republishes_jwt_key_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("pg_doorman.toml");
    let public_key = dir.path().join("jwt_public.pem");
    let old_private_key = dir.path().join("jwt_old_private.pem");
    let new_private_key = dir.path().join("jwt_new_private.pem");
    write_rsa_keypair(&public_key, &old_private_key);
    let public_key_str = public_key.to_str().unwrap();
    let config_path_str = config_path.to_str().unwrap();

    let config = format!(
        r#"
[general]
host = "127.0.0.1"
port = 6432
admin_username = "admin"
admin_password = "admin_password"

[pools.example_db]
server_host = "localhost"
server_port = 5432

[[pools.example_db.users]]
username = "jwt_user"
password = "jwt-pkey-fpath:{public_key_str}?iss=issuer&aud=audience"
pool_size = 5
"#
    );
    std::fs::write(&config_path, config).unwrap();

    parse(config_path_str).await.unwrap();
    let old_token = crate::auth::jwt::sign_with_jwt_priv_key(
        crate::auth::jwt::new_claims_with_scope(
            "jwt_user".to_string(),
            std::time::Duration::from_secs(60),
            "issuer".to_string(),
            "audience".to_string(),
        ),
        old_private_key.to_str().unwrap().to_string(),
    )
    .await
    .unwrap();
    crate::auth::jwt::get_user_name_from_jwt(
        public_key_str.to_string(),
        old_token,
        "issuer",
        "audience",
    )
    .await
    .expect("initial key must validate old token");

    write_rsa_keypair(&public_key, &new_private_key);
    let new_token = crate::auth::jwt::sign_with_jwt_priv_key(
        crate::auth::jwt::new_claims_with_scope(
            "jwt_user".to_string(),
            std::time::Duration::from_secs(60),
            "issuer".to_string(),
            "audience".to_string(),
        ),
        new_private_key.to_str().unwrap().to_string(),
    )
    .await
    .unwrap();

    let csm: ClientServerMap = std::sync::Arc::new(crate::utils::dashmap::new_dashmap(1));
    let changed = reload_config(csm).await.unwrap();

    assert!(
        !changed,
        "only key file bytes changed, config text stayed equal"
    );
    crate::auth::jwt::get_user_name_from_jwt(
        public_key_str.to_string(),
        new_token,
        "issuer",
        "audience",
    )
    .await
    .expect("unchanged reload must publish rotated JWT key bytes");
}

#[test]
fn web_section_sso_defaults() {
    let toml_str = r#"
host = "0.0.0.0"
port = 9127
enabled = false
ui = false
ui_anonymous = false
log_tap_max_entries = 8192
"#;
    let web: crate::config::web::Web = toml::from_str(toml_str).unwrap();
    assert!(!web.sso_enabled);
    assert!(web.sso_proxy_url.is_none());
    assert!(web.sso_public_key_file.is_none());
    assert!(web.sso_audience.is_empty());
    assert_eq!(web.sso_allowed_users, vec!["*".to_string()]);
}

#[test]
fn web_section_round_trips_sso_fields() {
    let toml_str = r#"
host = "0.0.0.0"
port = 9127
enabled = true
ui = true
ui_anonymous = false
log_tap_max_entries = 8192
sso_enabled = true
sso_proxy_url = "https://sso.example.com/oauth2/start"
sso_public_key_file = "/etc/pg_doorman/sso.pem"
sso_audience = ["pg_doorman"]
sso_allowed_users = ["alice", "bob"]
"#;
    let web: crate::config::web::Web = toml::from_str(toml_str).unwrap();
    assert!(web.sso_enabled);
    assert_eq!(
        web.sso_proxy_url.as_deref(),
        Some("https://sso.example.com/oauth2/start")
    );
    assert_eq!(
        web.sso_public_key_file.as_deref().and_then(|p| p.to_str()),
        Some("/etc/pg_doorman/sso.pem")
    );
    assert_eq!(web.sso_audience, vec!["pg_doorman".to_string()]);
    assert_eq!(
        web.sso_allowed_users,
        vec!["alice".to_string(), "bob".to_string()]
    );
}

#[tokio::test]
async fn reject_reserved_in_general_startup_parameters() {
    let mut cfg = Config::default();
    cfg.general
        .startup_parameters
        .insert("user".to_string(), "x".to_string());
    let err = cfg.validate().await.unwrap_err();
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("general.startup_parameters") && msg.contains("reserved"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn reject_reserved_in_pool_startup_parameters() {
    let mut cfg = Config::default();
    use_strong_admin_password(&mut cfg);
    cfg.general.tls_rate_limit_per_second = 0;
    let mut pool = Pool::default();
    pool.startup_parameters
        .insert("database".to_string(), "x".to_string());
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    cfg.pools.insert("p".to_string(), pool);
    let err = cfg.validate().await.unwrap_err();
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("pool.startup_parameters") && msg.contains("reserved"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn reject_merged_general_pool_startup_parameters_overflow() {
    // Two layers can fit on their own and still overflow once merged.
    // Config validation should catch that at `pg_doorman -t`, before the
    // first client tries to connect.
    let mut cfg = Config::default();
    cfg.general.tls_rate_limit_per_second = 0;
    let filler = "x".repeat(4800);
    cfg.general
        .startup_parameters
        .insert("aaa_big".to_string(), filler.clone());
    let mut pool = Pool::default();
    pool.startup_parameters
        .insert("bbb_big".to_string(), filler);
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    cfg.pools.insert("p".to_string(), pool);
    let err = cfg.validate().await.unwrap_err();
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("merged general + pools.p.startup_parameters")
                && msg.contains("exceeds operator budget"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

/// Helper for the release_query validation cases below. Builds a
/// minimal-but-valid pool and lets the caller override `release_query`.
fn release_query_pool(release_query: Option<String>) -> Pool {
    let mut pool = Pool::default();
    pool.release_query = release_query;
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    pool
}

#[tokio::test]
async fn validate_release_query_none_is_accepted() {
    // Omitted (=None) means "use the iServ-compatible default" and must pass.
    let mut pool = release_query_pool(None);
    pool.validate().await.expect("None release_query must pass");
}

#[tokio::test]
async fn validate_release_query_empty_is_accepted() {
    // Empty string is the explicit "disabled" sentinel and must pass.
    let mut pool = release_query_pool(Some(String::new()));
    pool.validate()
        .await
        .expect("empty release_query disables the feature and must pass");
}

#[tokio::test]
async fn validate_release_query_rejects_whitespace_only() {
    // " \n\t" looks intentional but executes as an empty statement on PG; the
    // operator likely meant to disable it. Reject so the typo is loud.
    let mut pool = release_query_pool(Some("  \n\t  ".to_string()));
    let err = pool
        .validate()
        .await
        .expect_err("whitespace-only release_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("release_query") && msg.contains("whitespace"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_release_query_rejects_oversized() {
    // 4096 bytes is the same cap the iServ patch used. Anything beyond that
    // is almost certainly a paste error, not real SQL.
    let oversized = "X".repeat(4097);
    let mut pool = release_query_pool(Some(oversized));
    let err = pool
        .validate()
        .await
        .expect_err("release_query > 4096 bytes must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("release_query") && msg.contains("4096"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_prewarm_query_empty_is_accepted() {
    // Empty (the serde-default for `String`) means "no prewarm" and must pass.
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    pool.validate()
        .await
        .expect("empty prewarm_query is the disabled state and must pass");
}

#[tokio::test]
async fn validate_prewarm_query_rejects_whitespace_only() {
    let mut pool = Pool::default();
    pool.prewarm_query = "  \n  ".to_string();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("whitespace-only prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("whitespace"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_release_query_rejects_null_byte() {
    // regression: PostgreSQL simple-query frames terminate at the first
    // NUL byte. A literal `\0` in operator-configured SQL would cause the
    // backend to treat the bytes after as a new wire message -> protocol desync
    // -> mark_bad on every checkin -> pool silently empties. Mirrors the
    // matching guard already in src/config/startup_parameters.rs.
    let mut pool = release_query_pool(Some("SELECT 1;\0SELECT 2".to_string()));
    let err = pool
        .validate()
        .await
        .expect_err("release_query with embedded \\0 must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("release_query") && msg.contains("null byte"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_release_query_rejects_session_state_set_config() {
    let mut pool = release_query_pool(Some(
        "SELECT set_config('client.app_user', 'release', false)".to_string(),
    ));
    let err = pool
        .validate()
        .await
        .expect_err("session-scoped set_config in release_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("release_query") && msg.contains("set_config"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_release_query_rejects_reset_session_state() {
    for query in [
        "RESET ALL",
        "SELECT 1; RESET client.app_user",
        "SELECT 1; DISCARD ALL",
        "SELECT 1; DISCARD/* trace */ALL",
        "DISCARD -- trace\nALL",
    ] {
        let mut pool = release_query_pool(Some(query.to_string()));
        let err = pool
            .validate()
            .await
            .expect_err("session-state RESET in release_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("release_query") && msg.contains("session state"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_release_query_rejects_opaque_procedural_sql() {
    for query in [
        "DO $$ BEGIN PERFORM set_config('search_path','tenant',false); END $$",
        "CALL reset_session_state()",
    ] {
        let mut pool = release_query_pool(Some(query.to_string()));
        let err = pool
            .validate()
            .await
            .expect_err("opaque procedural release_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("release_query") && msg.contains("procedural"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_release_query_rejects_untracked_session_mutators() {
    for query in [
        "BEGIN",
        "LISTEN tenant_channel",
        "SELECT pg_advisory_lock(42)",
        "SELECT tenant_reset()",
        "WITH reset AS (SELECT tenant_reset()) SELECT 1",
    ] {
        let mut pool = release_query_pool(Some(query.to_string()));
        let err = pool
            .validate()
            .await
            .expect_err("untracked release_query session mutator must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("release_query") && msg.contains("session state"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_release_query_allows_local_set_config() {
    let mut pool = release_query_pool(Some(
        "SELECT set_config('work_mem', '64MB', true)".to_string(),
    ));
    pool.validate()
        .await
        .expect("LOCAL set_config in release_query must not be treated as session state");
}

#[tokio::test]
async fn validate_release_query_allows_known_cleanup_functions() {
    let mut pool = release_query_pool(Some(
        "SELECT pg_advisory_unlock_all(); SELECT public.pgv_free()".to_string(),
    ));
    pool.validate()
        .await
        .expect("known release cleanup functions must remain valid for custom release_query");
}

#[tokio::test]
async fn validate_release_query_allows_exact_known_cleanup_function_targets() {
    let mut pool = release_query_pool(Some(
        "SELECT pg_catalog.pg_advisory_unlock_all(); \
         SELECT public.pgv_free(); \
         SELECT pg_catalog.set_config('work_mem', '64MB', true)"
            .to_string(),
    ));
    pool.validate()
        .await
        .expect("exact known cleanup function targets must remain valid");
}

#[tokio::test]
async fn validate_release_query_allows_combined_cleanup_select() {
    // Both trusted cleanup functions in one SELECT target list (the iServ
    // default form) must validate, not just one-function-per-statement.
    let mut pool = release_query_pool(Some(
        "SELECT pg_catalog.pg_advisory_unlock_all(), public.pgv_free()".to_string(),
    ));
    pool.validate()
        .await
        .expect("combined trusted cleanup functions in one SELECT must be valid");
}

#[tokio::test]
async fn validate_release_query_rejects_qualified_trusted_function_spoofs() {
    for query in [
        "SELECT attacker_schema.set_config('work_mem', '64MB', true)",
        r#"SELECT "attacker_schema".set_config('work_mem', '64MB', true)"#,
        r#"SELECT attacker_schema."set_config"('work_mem', '64MB', true)"#,
        r#"SELECT "attacker_schema"."set_config"('work_mem', '64MB', true)"#,
        "SELECT attacker_schema.pg_advisory_unlock_all()",
        r#"SELECT "attacker_schema".pg_advisory_unlock_all()"#,
        "SELECT attacker_schema.pgv_free()",
        r#"SELECT "attacker_schema".pgv_free()"#,
    ] {
        let mut pool = release_query_pool(Some(query.to_string()));
        let err = pool
            .validate()
            .await
            .expect_err("qualified trusted-name spoof in release_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("release_query")
                    && (msg.contains("function") || msg.contains("set_config")),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_release_query_rejects_trusted_function_overload_shapes() {
    for query in [
        "SELECT set_config('work_mem', '64MB', true, 'extra')",
        "SELECT pg_catalog.set_config('work_mem', '64MB', true, 'extra')",
        "SELECT pg_advisory_unlock_all(1)",
        "SELECT pg_catalog.pg_advisory_unlock_all(1)",
        "SELECT public.pgv_free(1)",
    ] {
        let mut pool = release_query_pool(Some(query.to_string()));
        let err = pool
            .validate()
            .await
            .expect_err("trusted cleanup functions must require exact signatures");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("release_query")
                    && (msg.contains("function") || msg.contains("set_config")),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_null_byte() {
    // Same regression on the pool-level prewarm_query.
    let mut pool = Pool::default();
    pool.prewarm_query = "SELECT 1\0extra".to_string();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("prewarm_query with embedded \\0 must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("null byte"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_session_state_set() {
    let mut pool = Pool::default();
    pool.prewarm_query = "SET search_path = app, public".to_string();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("SET prewarm_query would be erased by cleanup and must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("SET"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_session_state_set_config() {
    let mut pool = Pool::default();
    pool.prewarm_query = "SELECT set_config('client.app_user', 'seed', false)".to_string();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("session set_config in prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("set_config"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_session_state_reset() {
    for query in [
        "RESET ALL",
        "SELECT 1; RESET ROLE",
        "SELECT 1; DISCARD ALL",
        "DISCARD/* trace */ALL",
    ] {
        let mut pool = Pool::default();
        pool.prewarm_query = query.to_string();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("session-state cleanup in prewarm_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("prewarm_query") && msg.contains("session state"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_opaque_procedural_sql() {
    for query in [
        "DO $$ BEGIN PERFORM set_config('search_path','tenant',false); END $$",
        "CALL reset_session_state()",
    ] {
        let mut pool = Pool::default();
        pool.prewarm_query = query.to_string();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("opaque procedural prewarm_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("prewarm_query") && msg.contains("procedural"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_transaction_control() {
    for query in ["BEGIN", "START TRANSACTION", "SAVEPOINT before_prewarm"] {
        let mut pool = Pool::default();
        pool.prewarm_query = query.to_string();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("transaction-control prewarm_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("prewarm_query") && msg.contains("session state"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_function_wrapped_session_state() {
    for query in [
        "SELECT tenant_reset()",
        "WITH reset AS (SELECT tenant_reset()) SELECT 1",
    ] {
        let mut pool = Pool::default();
        pool.prewarm_query = query.to_string();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("function-wrapped prewarm_query session state must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("prewarm_query")
                    && (msg.contains("function") || msg.contains("set_config")),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_quoted_session_state_set_config() {
    let mut pool = Pool::default();
    pool.prewarm_query =
        r#"SELECT pg_catalog."set_config"('client.app_user', 'seed', false)"#.to_string();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("quoted session set_config in prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("set_config"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_unicode_escaped_session_state_set_config() {
    let mut pool = Pool::default();
    pool.prewarm_query =
        r#"SELECT pg_catalog.U&"s\0065t_config"('client.app_user', 'seed', false)"#.to_string();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("Unicode-escaped session set_config in prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("set_config"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_prewarm_query_allows_local_set_config() {
    let mut pool = Pool::default();
    pool.prewarm_query = "SELECT set_config('work_mem', '64MB', true)".to_string();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    pool.validate()
        .await
        .expect("LOCAL set_config in prewarm_query must not be treated as session state");
}

#[tokio::test]
async fn validate_prewarm_query_rejects_qualified_set_config_spoof() {
    for query in [
        "SELECT attacker_schema.set_config('work_mem', '64MB', true)",
        r#"SELECT "attacker_schema".set_config('work_mem', '64MB', true)"#,
    ] {
        let mut pool = Pool::default();
        pool.prewarm_query = query.to_string();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("qualified set_config spoof in prewarm_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("prewarm_query")
                    && (msg.contains("function") || msg.contains("set_config")),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_trusted_function_overload_shapes() {
    for query in [
        "SELECT set_config('work_mem', '64MB', true, 'extra')",
        "SELECT pg_advisory_unlock_all(1)",
    ] {
        let mut pool = Pool::default();
        pool.prewarm_query = query.to_string();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("trusted cleanup functions in prewarm_query must require exact signatures");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("prewarm_query")
                    && (msg.contains("function") || msg.contains("set_config")),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_null_byte() {
    // Same regression on the per-user prewarm_query override.
    let user = User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some("SELECT 1\0junk".to_string()),
        ..User::default()
    };
    let err = user
        .validate()
        .await
        .expect_err("user-level prewarm_query with \\0 must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("null byte"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_session_state_set() {
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some("/* prefix */ SET ROLE app_role".to_string()),
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("user-level SET prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("user 'u' prewarm_query") && msg.contains("SET"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_session_state_set_config() {
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some(
            "/* prefix */ SELECT set_config('client.app_user','seed',false)".to_string(),
        ),
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("user-level session set_config prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("user 'u' prewarm_query") && msg.contains("set_config"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_qualified_set_config_spoof() {
    for query in [
        "SELECT attacker_schema.set_config('work_mem', '64MB', true)",
        r#"SELECT "attacker_schema".set_config('work_mem', '64MB', true)"#,
    ] {
        let mut pool = Pool::default();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            prewarm_query: Some(query.to_string()),
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("user-level qualified set_config spoof prewarm_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("user 'u' prewarm_query")
                    && (msg.contains("function") || msg.contains("set_config")),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_trusted_function_overload_shapes() {
    for query in [
        "SELECT set_config('work_mem', '64MB', true, 'extra')",
        "SELECT pg_advisory_unlock_all(1)",
    ] {
        let mut pool = Pool::default();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            prewarm_query: Some(query.to_string()),
            ..User::default()
        });
        let err = pool.validate().await.expect_err(
            "trusted cleanup functions in user prewarm_query must require exact signatures",
        );
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("user 'u' prewarm_query")
                    && (msg.contains("function") || msg.contains("set_config")),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_session_state_reset() {
    for query in [
        "RESET SESSION AUTHORIZATION",
        "SELECT 1; DISCARD -- trace\nALL",
    ] {
        let mut pool = Pool::default();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            prewarm_query: Some(query.to_string()),
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("user-level cleanup prewarm_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("user 'u' prewarm_query") && msg.contains("session state"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_opaque_procedural_sql() {
    for query in [
        "DO $$ BEGIN PERFORM set_config('search_path','tenant',false); END $$",
        "CALL reset_session_state()",
    ] {
        let mut pool = Pool::default();
        pool.users.push(User {
            username: "u".to_string(),
            password: "p".to_string(),
            pool_size: 1,
            prewarm_query: Some(query.to_string()),
            ..User::default()
        });
        let err = pool
            .validate()
            .await
            .expect_err("opaque procedural user-level prewarm_query must be rejected");
        match err {
            Error::BadConfig(msg) => assert!(
                msg.contains("user 'u' prewarm_query") && msg.contains("procedural"),
                "unexpected message for {query:?}: {msg}"
            ),
            other => panic!("expected BadConfig for {query:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_function_wrapped_session_state() {
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some("SELECT tenant_reset()".to_string()),
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("user-level function-wrapped prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("user 'u' prewarm_query") && msg.contains("function"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_quoted_session_state_set_config() {
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some(
            r#"/* prefix */ SELECT pg_catalog."set_config"('client.app_user','seed',false)"#
                .to_string(),
        ),
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("user-level quoted session set_config prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("user 'u' prewarm_query") && msg.contains("set_config"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_unicode_escaped_session_state_set_config() {
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some(
            r#"/* prefix */ SELECT pg_catalog.U&"s\0065t_config"('client.app_user','seed',false)"#
                .to_string(),
        ),
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("user-level Unicode-escaped session set_config prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("user 'u' prewarm_query") && msg.contains("set_config"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_prewarm_query_rejects_oversized() {
    let mut pool = Pool::default();
    pool.prewarm_query = "X".repeat(4097);
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("prewarm_query > 4096 bytes must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("4096"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_user_prewarm_query_empty_string_is_accepted() {
    // `Some(String::new())` is the explicit "disable for this user" sentinel.
    let mut pool = Pool::default();
    pool.prewarm_query = "SELECT 1".to_string();
    pool.users.push(User {
        username: "muted".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some(String::new()),
        ..User::default()
    });
    pool.validate()
        .await
        .expect("user-level empty prewarm_query disables the override and must pass");
}

#[tokio::test]
async fn default_pool_intercepts_discard_all() {
    // The iServ contract relied on by production workloads (long-lived
    // shared CREATE TEMP TABLE) requires this default to STAY true.
    // Treat this as a guard - a refactor that flipped the default
    // would silently start forwarding DISCARD ALL to PostgreSQL and
    // wipe the deliberately-shared temp tables across pooled clients.
    let pool = Pool::default();
    assert!(pool.intercept_discard_all);
    assert!(Pool::default_intercept_discard_all());
}

#[tokio::test]
async fn pool_intercept_discard_all_can_be_disabled_per_pool() {
    // Opt-out path for applications that need real DISCARD ALL semantics
    // (UNLISTEN, ON COMMIT DROP temp tables, two-phase commits).
    let mut pool = Pool::default();
    pool.intercept_discard_all = false;
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        ..User::default()
    });
    pool.validate()
        .await
        .expect("opting out of DISCARD ALL interception must validate cleanly");
    assert!(!pool.intercept_discard_all);
}

#[tokio::test]
async fn validate_user_prewarm_query_rejects_whitespace_only() {
    let mut pool = Pool::default();
    pool.users.push(User {
        username: "u".to_string(),
        password: "p".to_string(),
        pool_size: 1,
        prewarm_query: Some("\t \n".to_string()),
        ..User::default()
    });
    let err = pool
        .validate()
        .await
        .expect_err("user-level whitespace-only prewarm_query must be rejected");
    match err {
        Error::BadConfig(msg) => assert!(
            msg.contains("prewarm_query") && msg.contains("whitespace") && msg.contains("user 'u'"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected BadConfig, got {other:?}"),
    }
}

/// exercise the client-facing TLS diff helper so
/// the reload-time warning gate cannot regress silently. Mirrors
/// `app/tls.rs:14` `init_tls` semantics: any change to the five
/// fields below cannot be picked up by SIGHUP.
#[test]
fn client_facing_tls_fields_differ_flags_each_field() {
    let base = General::default();

    // 1. equal snapshots -> no diff
    assert!(!client_facing_tls_fields_differ(&base, &base));

    // 2. tls_certificate
    let mut changed = base.clone();
    changed.tls_certificate = Some("/etc/new.crt".to_string());
    assert!(client_facing_tls_fields_differ(&base, &changed));

    // 3. tls_private_key
    let mut changed = base.clone();
    changed.tls_private_key = Some("/etc/new.key".to_string());
    assert!(client_facing_tls_fields_differ(&base, &changed));

    // 4. tls_ca_cert
    let mut changed = base.clone();
    changed.tls_ca_cert = Some("/etc/new-ca.crt".to_string());
    assert!(client_facing_tls_fields_differ(&base, &changed));

    // 5. tls_mode
    let mut changed = base.clone();
    changed.tls_mode = Some("require".to_string());
    assert!(client_facing_tls_fields_differ(&base, &changed));

    // 6. tls_rate_limit_per_second
    let mut changed = base.clone();
    changed.tls_rate_limit_per_second = base.tls_rate_limit_per_second + 1;
    assert!(client_facing_tls_fields_differ(&base, &changed));

    // 7. unrelated field change must NOT trigger the warning
    let mut changed = base.clone();
    changed.server_lifetime = crate::config::duration::Duration(99_000);
    assert!(!client_facing_tls_fields_differ(&base, &changed));
}

#[test]
fn restart_only_listener_fields_changed_flags_each_bind_field() {
    let base = Config::default();

    assert!(restart_only_listener_fields_changed(&base, &base).is_empty());

    let mut changed = base.clone();
    changed.general.host = "127.0.0.1".to_string();
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.host"]
    );

    let mut changed = base.clone();
    changed.general.port += 1;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.port"]
    );

    let mut changed = base.clone();
    changed.general.unix_socket_dir = Some("/tmp/pg_doorman".to_string());
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.unix_socket_dir"]
    );

    let mut changed = base.clone();
    changed.general.unix_socket_mode = "0660".to_string();
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.unix_socket_mode"]
    );

    let mut changed = base.clone();
    changed.general.backlog += 1;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.backlog"]
    );

    let mut changed = base.clone();
    changed.general.tls_certificate = Some("/etc/new.crt".to_string());
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.tls_certificate"]
    );

    let mut changed = base.clone();
    changed.general.tls_private_key = Some("/etc/new.key".to_string());
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.tls_private_key"]
    );

    let mut changed = base.clone();
    changed.general.tls_ca_cert = Some("/etc/new-ca.crt".to_string());
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.tls_ca_cert"]
    );

    let mut changed = base.clone();
    changed.general.tls_mode = Some("require".to_string());
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.tls_mode"]
    );

    let mut changed = base.clone();
    changed.general.tls_rate_limit_per_second = base.general.tls_rate_limit_per_second + 100;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.tls_rate_limit_per_second"]
    );

    let mut changed = base.clone();
    changed.general.worker_threads += 1;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.worker_threads"]
    );

    let mut changed = base.clone();
    changed.general.worker_cpu_affinity_pinning = !base.general.worker_cpu_affinity_pinning;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.worker_cpu_affinity_pinning"]
    );

    let mut changed = base.clone();
    changed.general.worker_stack_size = Some(crate::config::byte_size::ByteSize(8 * 1024 * 1024));
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.worker_stack_size"]
    );

    let mut changed = base.clone();
    changed.general.max_blocking_threads = Some(64);
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.max_blocking_threads"]
    );

    let mut changed = base.clone();
    changed.general.tokio_global_queue_interval = Some(5);
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.tokio_global_queue_interval"]
    );

    let mut changed = base.clone();
    changed.general.tokio_event_interval = Some(1);
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.tokio_event_interval"]
    );

    let mut changed = base.clone();
    changed.general.query_interner_gc_interval_seconds += 1;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.query_interner_gc_interval_seconds"]
    );

    let mut changed = base.clone();
    changed.general.retain_connections_time = crate::config::duration::Duration::from_millis(
        base.general.retain_connections_time.as_millis() + 1,
    );
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.retain_connections_time"]
    );

    let mut changed = base.clone();
    changed.general.retain_connections_max += 1;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["general.retain_connections_max"]
    );

    let mut changed = base.clone();
    changed.web.enabled = !base.web.enabled;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["web.enabled"]
    );

    let mut changed = base.clone();
    changed.web.host = "127.0.0.1".to_string();
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["web.host"]
    );

    let mut changed = base.clone();
    changed.web.port += 1;
    assert_eq!(
        restart_only_listener_fields_changed(&base, &changed),
        vec!["web.port"]
    );

    let mut changed = base.clone();
    changed.general.server_lifetime = crate::config::duration::Duration(99_000);
    assert!(restart_only_listener_fields_changed(&base, &changed).is_empty());
}
