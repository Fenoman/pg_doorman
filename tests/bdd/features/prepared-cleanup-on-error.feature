@rust @rust-2 @prepared-cleanup-on-error
Feature: An ordinary SQL error keeps the backend's prepared statements
  DEALLOCATE ALL at check-in drops every statement on the backend,
  including the DOORMAN_N shared by other clients, and with
  cleanup_server_connections = false the backend is closed instead. Only
  errors showing that the pooler's view of the backend's statements is
  stale (0A000, 26000, 42P05) may schedule it.

  Background:
    Given PostgreSQL started with pg_hba.conf:
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
      prepared_statements = true

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1

      [pools.nocleanup_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      cleanup_server_connections = false

      [[pools.nocleanup_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  Scenario: A statement error does not drop the backend's prepared statements
    When we create session "one" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send Parse "q" with query "SELECT 1" to session "one"
    And we send Bind "" to "q" with params "" to session "one"
    And we send Execute "" to session "one"
    And we send Sync to session "one"
    And we send SimpleQuery "SELECT 1/0" to session "one" expecting error
    Then session "one" should receive error containing "division by zero"
    When we send SimpleQuery "SELECT count(*) FROM pg_prepared_statements WHERE name LIKE 'DOORMAN%' AND name NOT IN ('DOORMAN_release_begin', 'DOORMAN_release', 'DOORMAN_release_commit')" to session "one" and store response
    Then session "one" should receive DataRow with "1"

  Scenario: With cleanup disabled a statement error keeps the backend
    When we create session "one" to pg_doorman as "example_user_1" with password "" and database "nocleanup_db"
    And we send Parse "q" with query "SELECT 1" to session "one"
    And we send Bind "" to "q" with params "" to session "one"
    And we send Execute "" to session "one"
    And we send Sync to session "one"
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "one" and store backend_pid as "before"
    And we send SimpleQuery "SELECT 1/0" to session "one" expecting error
    Then session "one" should receive error containing "division by zero"
    When we send SimpleQuery "SELECT pg_backend_pid()" to session "one" and store backend_pid as "after"
    Then named backend_pid "after" from session "one" is same as "before"

  Scenario: A statement error in a Flush pipeline keeps the backend's prepared statements
    When we create session "one" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send Parse "q" with query "SELECT 1" to session "one"
    And we send Bind "" to "q" with params "" to session "one"
    And we send Execute "" to session "one"
    And we send Sync to session "one"
    And we send Parse "" with query "SELECT 1/0" to session "one"
    And we send Bind "" to "" with params "" to session "one"
    And we send Execute "" to session "one"
    And we send Flush to session "one"
    And we send Sync to session "one"
    Then session "one" should receive error containing "division by zero"
    When we send SimpleQuery "SELECT count(*) FROM pg_prepared_statements WHERE statement = 'SELECT 1'" to session "one" and store response
    Then session "one" should receive DataRow with "1"
