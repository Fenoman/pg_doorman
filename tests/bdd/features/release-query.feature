@rust @rust-3 @release-query
Feature: Configurable release query
  The release query runs after checkin cleanup when a backend connection is
  returned to the pool. It must be configurable without changing session or
  transaction pooling semantics.

  Background:
    Given PostgreSQL started with options "-c log_statement=all -c logging_collector=off" and pg_hba.conf:
      """
      local all all trust
      host all all 127.0.0.1/32 trust
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

      [pools.release_tx]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = "SELECT 42 AS pgdoorman_release_tx_marker"

      [[pools.release_tx.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.release_session]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "session"
      release_query = "SELECT 43 AS pgdoorman_release_session_marker"

      [[pools.release_session.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.release_disabled]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""

      [[pools.release_disabled.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.release_failing]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = "SELECT 1 / (SELECT denominator FROM release_failure_control)"

      [[pools.release_failing.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1

      [pools.release_blocked]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = "SELECT denominator FROM release_failure_control"

      [[pools.release_blocked.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  Scenario: custom release_query runs in transaction mode
    When we create session "tx" to pg_doorman as "example_user_1" with password "" and database "release_tx"
    And we truncate PostgreSQL log
    And we send SimpleQuery "SELECT 1" to session "tx"
    And we sleep 300ms
    Then PostgreSQL log should contain "pgdoorman_release_tx_marker"

  Scenario: custom release_query runs when a session-mode backend is released
    When we create session "session" to pg_doorman as "example_user_1" with password "" and database "release_session"
    And we truncate PostgreSQL log
    And we send SimpleQuery "SELECT 1" to session "session"
    And we sleep 300ms
    Then PostgreSQL log should not contain "pgdoorman_release_session_marker"
    When we close session "session"
    And we sleep 300ms
    Then PostgreSQL log should contain "pgdoorman_release_session_marker"

  Scenario: empty release_query disables release cleanup
    When we create session "disabled" to pg_doorman as "example_user_1" with password "" and database "release_disabled"
    And we truncate PostgreSQL log
    And we send SimpleQuery "SELECT 1" to session "disabled"
    And we sleep 300ms
    Then PostgreSQL log should not contain "pg_advisory_unlock_all"
    And PostgreSQL log should not contain "pgv_free"

  @release-query-failure-result
  Scenario: release_query failure does not hide a committed result or disconnect the client
    When we create session "failing" to pg_doorman as "example_user_1" with password "" and database "release_failing"
    And we send SimpleQuery "CREATE TABLE release_result_once(id integer PRIMARY KEY); WITH armed AS (UPDATE release_failure_control SET denominator = 0 RETURNING denominator), inserted AS (INSERT INTO release_result_once VALUES (1) RETURNING id) SELECT inserted.id FROM inserted CROSS JOIN armed" to session "failing" and store response
    Then session "failing" should receive DataRow with "1"
    When we send SimpleQuery "SELECT count(*) FROM release_result_once" to session "failing" and store response
    Then session "failing" should receive DataRow with "1"

  @release-query-response-before-cleanup
  Scenario: client receives the completed query before blocked release cleanup finishes
    When we create session "slow" to pg_doorman as "example_user_1" with password "" and database "release_blocked" and store backend key
    And we create session "blocker" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN; LOCK TABLE release_failure_control IN ACCESS EXCLUSIVE MODE" to session "blocker" and store response
    And we send SimpleQuery "SELECT 42" to session "slow" without waiting
    Then we read SimpleQuery response from session "slow" within 500ms
    And session "slow" should receive DataRow with "42"
    When we send cancel request for session "slow"
    And we send SimpleQuery "COMMIT" to session "blocker" and store response
    And we send SimpleQuery "SELECT 43" to session "slow" and store response
    Then session "slow" should receive DataRow with "43"
