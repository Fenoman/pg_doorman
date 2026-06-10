@python @cancel-safe
Feature: psycopg3 cancel_safe() / libpq 17+ PQcancelConn through pg_doorman without TLS

  Regression for the post-SSL-rejected CancelRequest bug.

  psycopg3's `Connection.cancel_safe()` uses `PQcancelConn` from libpq 17+.
  With the default `sslmode=prefer` the cancel socket first sends an
  `SSLRequest`. When pg_doorman is configured WITHOUT TLS (no
  `tls_private_key`/`tls_certificate`), it replies `'N'` and reads the next
  startup-class message. libpq then sends `CancelRequest` over the same
  plain socket - a legitimate path per the PostgreSQL wire protocol.

  Before the fix pg_doorman dropped that CancelRequest with
  `ProtocolSyncError("Unexpected protocol message during plain-text startup
  negotiation")`, surfacing at libpq as:
      `cancellation failed: ... server closed the connection unexpectedly`
  and leaving the long-running query alive on the backend.

  Note: `cancel-query-libpq-noise.feature` exists but configures TLS - that
  path negotiates a full SSL handshake on the cancel socket and exercises
  the post-TLS cancel arm in `startup.rs::startup_tls`, which was never
  broken. This feature deliberately leaves TLS unconfigured to exercise the
  formerly-broken arm in `entrypoint.rs`.

  Background:
    Given PostgreSQL started with pg_hba.conf:
      """
      local   all             all                                     trust
      host    all             all             127.0.0.1/32            trust
      host    all             all             ::1/128                 trust
      """
    And fixtures from "tests/fixture.sql" applied
    And pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      connect_timeout = 5000
      admin_username = "admin"
      admin_password = "admin"
      pg_hba.content = "host all all 127.0.0.1/32 trust"

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """

  Scenario: psycopg3 cancel_safe and legacy cancel both succeed through pg_doorman
    When I run shell command:
      """
      cd tests/python && \
      export DATABASE_URL="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db" && \
      pytest -v ./test_cancel_safe.py
      """
    Then the command should succeed
