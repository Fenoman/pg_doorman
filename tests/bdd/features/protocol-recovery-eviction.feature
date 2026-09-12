@rust @rust-1 @protocol-recovery @protocol-recovery-eviction
Feature: Rejected backend registrations preserve confirmed logical aliases
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
      server_prepared_statements_cache_size = 1
      client_anonymous_prepared_cache_size = 1
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

  Scenario: An error-skipped Parse sharing an evicted backend alias cannot erase confirmed names
    Then extended error case "evicted backend" preserves the PostgreSQL statement namespace

  Scenario: A cold Bind uses backend acknowledgements in actual send order
    Then a cold Bind after an error-skipped batch remains usable after savepoint rollback
