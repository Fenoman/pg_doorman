@rust @rust-2 @sql-prepare-disabled-cache
Feature: Statements created with SQL PREPARE keep the backend with the statement cache disabled
  With prepared_statements = false a named Parse reaches PostgreSQL under
  the client's own name. A SQL DEALLOCATE of such a statement must not
  release the backend while a statement created with SQL PREPARE remains.

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
      prepared_statements = false

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  Scenario: Deallocating a protocol statement keeps the statement created with SQL PREPARE
    When we create session "owner" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "PREPARE p AS SELECT 1" to session "owner"
    And we send Parse "r" with query "SELECT 2" to session "owner"
    And we send Sync to session "owner"
    And we send SimpleQuery "DEALLOCATE r" to session "owner"
    And we send SimpleQuery "EXECUTE p" to session "owner" and store response
    Then session "owner" should receive DataRow with "1"
