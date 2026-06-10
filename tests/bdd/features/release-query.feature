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
