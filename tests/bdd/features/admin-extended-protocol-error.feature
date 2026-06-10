@python @admin-extended
Feature: admin database returns ErrorResponse (not socket drop) on extended-query frames

  Regression. The admin handler matched the first
  message byte against `'Q'` only; everything else returned
  `Err(ProtocolSyncError(...))` and the admin dispatch path skips
  `process_error`, so the error never reached the wire and the socket
  was dropped silently. Drivers that default to the extended-query
  protocol (psycopg3, asyncpg, pgjdbc `simpleProtocolOnly=false`,
  npgsql) then surfaced
  `OperationalError: server closed the connection unexpectedly`
  on basic admin commands like `SHOW POOLS`.

  Same beeaea7 / ffae1e5 / a5187fb / 1bdc1a0 / 7bfde3e shape: a
  legitimate client request answered with a protocol-correct typed
  ErrorResponse (SQLSTATE 0A000 - feature_not_supported) instead of
  a silent drop.

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

  Scenario: raw extended-protocol Parse frame against admin db returns 0A000 ErrorResponse
    When I run shell command:
      """
      cd tests/python && \
      export ADMIN_DSN="postgresql://admin:admin@127.0.0.1:${DOORMAN_PORT}/pgbouncer" && \
      python3 -m pytest -v ./test_admin_extended_proto.py
      """
    Then the command should succeed
