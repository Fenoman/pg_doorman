@rust @rust-2 @sql-prepare-namespace
Feature: Statements created with SQL PREPARE behave as on PostgreSQL
  In transaction pooling a client that runs SQL PREPARE keeps its backend
  while its statements exist there. Commands addressing those statements
  must reach PostgreSQL under the client's names, and commands addressing
  the pooler's own DOORMAN_* aliases must not reach them.

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
    When we login to postgres and pg_doorman as "example_user_1" with password "" and database "example_db"

  Scenario: A protocol Close deallocates a statement created with SQL PREPARE
    When we send SimpleQuery "PREPARE sql_prepared AS SELECT 1" to both
    And we send Close "S" "sql_prepared" to both
    And we send Sync to both
    And we send SimpleQuery "PREPARE sql_prepared AS SELECT 2" to both
    Then we should receive identical messages from both
