@rust @rust-3 @prepared-cache @prepared-ddl-recovery
Feature: Shared prepared statement recovers after a result-shape DDL
  A repeated Parse of the same SQL reuses the backend's DOORMAN_N without a
  new parse analysis. After DDL that changes the result type, the first Bind
  of that statement fails with 0A000. The error schedules DEALLOCATE ALL at
  checkin, so the next Parse is prepared against the new schema.

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
      """

  Scenario: A stale shared statement fails once and the next Parse sees the new schema
    When we create session "ddl" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "CREATE TABLE ddl_recovery(a int); INSERT INTO ddl_recovery VALUES (7)" to session "ddl"
    And we create session "app" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send Parse "" with query "SELECT * FROM ddl_recovery" to session "app"
    And we send Bind "" to "" with params "" to session "app"
    And we send Execute "" to session "app"
    And we send Sync to session "app"
    Then session "app" should receive DataRow with "7"
    When we send SimpleQuery "SELECT pg_backend_pid()" to session "app" and store backend_pid as "before_ddl"
    # DDL from another connection changes the result type of the shared plan.
    And we send SimpleQuery "ALTER TABLE ddl_recovery ALTER COLUMN a TYPE text USING 'changed:' || a" to session "ddl"
    And we send Parse "" with query "SELECT * FROM ddl_recovery" to session "app"
    And we send Bind "" to "" with params "" to session "app"
    And we send Execute "" to session "app"
    And we send Sync to session "app"
    Then session "app" should receive ErrorResponse with SQLSTATE "0A000"
    # The failed cycle cleaned the same backend with DEALLOCATE ALL, so this
    # Parse reaches PostgreSQL and sees the new column type.
    When we send Parse "" with query "SELECT * FROM ddl_recovery" to session "app"
    And we send Bind "" to "" with params "" to session "app"
    And we send Execute "" to session "app"
    And we send Sync to session "app"
    Then session "app" should receive DataRow with "changed:7"
    # Recovery must come from the cleanup, not from replacing the backend.
    When we send SimpleQuery "SELECT pg_backend_pid()" to session "app" and store backend_pid as "after_recovery"
    Then named backend_pid "before_ddl" from session "app" is same as "after_recovery"
