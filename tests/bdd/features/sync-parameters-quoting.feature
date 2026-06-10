@rust @rust-3 @sync-parameters
Feature: sync_parameters uses dollar-quoted SQL literals
  When pg_doorman replays a client's session GUCs onto a freshly checked-out
  backend it builds `SET <key> TO <literal>` statements. Upstream used a
  single-quote literal with `''` escaping; the iServ backport switches to
  dollar-quoting so values containing apostrophes / backslashes / arbitrary
  punctuation reach PostgreSQL unmangled.

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
      sync_server_parameters = true
      pg_hba.content = "host all all 127.0.0.1/32 trust"

      [pools.sync_tx]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""

      [[pools.sync_tx.users]]
      username = "example_user_1"
      password = ""
      pool_size = 4
      """

  Scenario: application_name with apostrophes round-trips through sync_parameters
    # Two sessions push different application_names; the second will trigger
    # a sync_parameters cascade on a recycled backend. The value contains
    # an apostrophe - single-quote escaping would either error out or
    # truncate; dollar-quoting must keep it byte-exact.
    When we create session "a" to pg_doorman as "example_user_1" with password "" and database "sync_tx" and startup parameters "application_name=O'Brien"
    And we send SimpleQuery "SELECT 1" to session "a"
    And we sleep 200ms
    And we truncate PostgreSQL log
    When we create session "b" to pg_doorman as "example_user_1" with password "" and database "sync_tx" and startup parameters "application_name=quoted'thing"
    And we send SimpleQuery "SELECT 2" to session "b"
    And we sleep 200ms
    # The replayed SET must use $pgdoorman<N>$ dollar quoting, not '...''...'.
    Then PostgreSQL log should contain "$pgdoorman"
    And PostgreSQL log should contain "quoted'thing"
