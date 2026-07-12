@rust @rust-3 @discard-all-disabled-prepared
Feature: Extended DISCARD ALL with prepared statement caching disabled

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
      prepared_statements = false
      pg_hba.content = "host all all 127.0.0.1/32 trust"

      [pools.discard_disabled]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""

      [[pools.discard_disabled.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  Scenario: extended DISCARD ALL preserves backend session state
    When we create session "disabled" to pg_doorman as "example_user_1" with password "" and database "discard_disabled"
    And we send SimpleQuery "CREATE TEMP TABLE disabled_discard_guard(value integer); INSERT INTO disabled_discard_guard VALUES (1)" to session "disabled"
    And we truncate PostgreSQL log
    And we send Parse "" with query "DISCARD ALL" to session "disabled"
    And we send Bind "" to "" with params "" to session "disabled"
    And we send Execute "" to session "disabled"
    And we send Sync to session "disabled"
    And we send SimpleQuery "SELECT count(*) FROM disabled_discard_guard" to session "disabled" and store response
    Then session "disabled" should receive DataRow with "1"
    And PostgreSQL log should not contain "DISCARD ALL"
    And PostgreSQL log should contain "SELECT 1"
