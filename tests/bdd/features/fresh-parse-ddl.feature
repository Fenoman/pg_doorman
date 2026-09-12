@rust @rust-1 @fresh-parse-ddl
Feature: Fresh Parse observes schema changes without rewriting old statements
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
      server_prepared_statements_cache_size = 16
      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      release_query = ""
      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """
    When we login to postgres and pg_doorman as "example_user_1" with password "" and database "example_db"

  Scenario Outline: Fresh Parse and old Bind retain their separate DDL semantics
    Then a fresh "<name>" Parse observes DDL and rollback while an old Bind keeps its result shape
    Examples:
      | name      |
      | named     |
      | anonymous |
