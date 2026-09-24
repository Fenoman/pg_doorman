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
      query_wait_timeout = "2s"
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

  Scenario: Deallocating the last SQL statement returns the backend to the pool
    When we create session "owner" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "PREPARE p AS SELECT 1" to session "owner"
    And we send SimpleQuery "PREPARE q AS SELECT 2" to session "owner"
    And we send SimpleQuery "DEALLOCATE p" to session "owner"
    And we send SimpleQuery "DEALLOCATE q" to session "owner"
    And we create session "other" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT 1" to session "other" and store response
    Then session "other" should receive DataRow with "1"

  Scenario: A failed SQL PREPARE does not keep the backend
    When we create session "owner" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "PREPARE p AS SELECT * FROM missing_table" to session "owner" expecting error
    And we create session "other" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT 1" to session "other" and store response
    Then session "other" should receive DataRow with "1"

  Scenario: DEALLOCATE ALL answers like PostgreSQL and keeps the shared statements
    When we send SimpleQuery "DEALLOCATE ALL" to both
    Then we should receive identical messages from both
    When we create session "one" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send Parse "q" with query "SELECT 1" to session "one"
    And we send Bind "" to "q" with params "" to session "one"
    And we send Execute "" to session "one"
    And we send Sync to session "one"
    And we send SimpleQuery "DEALLOCATE ALL" to session "one" and store response
    Then session "one" should receive CommandComplete "DEALLOCATE ALL"
    When we send SimpleQuery "SELECT count(*) FROM pg_prepared_statements WHERE name LIKE 'DOORMAN%'" to session "one" and store response
    Then session "one" should receive DataRow with "1"

  Scenario: A DEALLOCATE ALL inside a pipeline keeps the statements prepared after it
    When we send Parse "reset" with query "DEALLOCATE ALL" to both
    And we send Bind "" to "reset" with params "" to both
    And we send Execute "" to both
    And we send Parse "a" with query "SELECT 1" to both
    And we send Bind "" to "a" with params "" to both
    And we send Execute "" to both
    And we send Sync to both
    And we send Bind "" to "a" with params "" to both
    And we send Execute "" to both
    And we send Sync to both
    Then we should receive identical messages from both

  Scenario: A DEALLOCATE ALL inside a pipeline that ends in an error keeps the statements prepared after it
    When we send Parse "reset" with query "DEALLOCATE ALL" to both
    And we send Bind "" to "reset" with params "" to both
    And we send Execute "" to both
    And we send Parse "a" with query "SELECT 1" to both
    And we send Bind "" to "a" with params "" to both
    And we send Execute "" to both
    And we send Parse "bad" with query "SELECT * FROM sql_prepare_namespace_missing" to both
    And we send Sync to both
    And we send Bind "" to "a" with params "" to both
    And we send Execute "" to both
    And we send Sync to both
    Then we should receive identical messages from both
