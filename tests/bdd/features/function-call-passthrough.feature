@python @function-call
Feature: forward FunctionCall ('F') instead of dropping the libpq lo_* API

  Regression. pg_doorman matched the client
  message-code byte against `Q/X/P/B/D/E/C/S/H/d/c/f` only; everything
  else (including the perfectly legal `'F'` FunctionCall frame from
  the PG protocol spec) fell through the `_ =>` arm into
  `Err(ProtocolSyncError(...))`, the outer dispatcher marked the
  backend bad, and the client was disconnected with
  "server closed the connection unexpectedly".

  libpq's entire large-object API (`lo_creat`, `lo_open`, `lo_read`,
  `lo_write`, `lo_close`, `lo_unlink`) is dispatched through `PQfn`
  which emits `'F'`. The previous behaviour broke every libpq-based
  application that touches blobs - psycopg2 `lobject()`, PHP
  `pg_lo_*`, Perl DBD::Pg `lo_*`, `pg_dump --large-objects`,
  legacy 1С bridges.

  Same beeaea7 / ffae1e5 / a5187fb pattern: a legitimate client
  request acknowledged as a protocol error.

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

  Scenario: psycopg2 lobject round-trip (PQfn -> 'F' frame) succeeds through pg_doorman
    When I run shell command:
      """
      cd tests/python && \
      export DATABASE_URL="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db" && \
      python3 -m pytest -v ./test_function_call.py
      """
    Then the command should succeed
