@rust @rust-3 @prewarm-query
Feature: Configurable prewarm query
  `prewarm_query` runs exactly once on a fresh backend right after startup,
  before the connection joins the idle pool. A SQL or transport failure must
  mark the backend bad so it never reaches the idle queue, and the per-pool
  `total_prewarm_failures` counter must surface the failure.

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

      [pools.prewarm_ok]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""
      prewarm_query = "SELECT 42 AS pgdoorman_prewarm_marker"

      [[pools.prewarm_ok.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.prewarm_disabled]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""

      [[pools.prewarm_disabled.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.prewarm_bad]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""
      prewarm_query = "SELECT * FROM pg_doorman_no_such_table_42"

      [[pools.prewarm_bad.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      """

  Scenario: prewarm_query runs once when a fresh backend is created
    # The first checkout creates the physical backend; the prewarm SQL must
    # land on the wire before the client's own query.
    When we truncate PostgreSQL log
    And we create session "warm" to pg_doorman as "example_user_1" with password "" and database "prewarm_ok"
    And we send SimpleQuery "SELECT 1" to session "warm"
    And we sleep 200ms
    # First session opened a fresh backend → prewarm marker must appear when
    # we wait long enough; checking that the marker EVER appears in the log.
    When we close session "warm"
    And we create session "warm2" to pg_doorman as "example_user_1" with password "" and database "prewarm_ok"
    And we sleep 200ms
    Then PostgreSQL log should contain "pgdoorman_prewarm_marker"

  Scenario: prewarm_query is NOT executed when omitted
    # No prewarm config in this pool - backend must never see the marker
    # from the other pool's config.
    When we create session "plain" to pg_doorman as "example_user_1" with password "" and database "prewarm_disabled"
    And we truncate PostgreSQL log
    And we send SimpleQuery "SELECT 1" to session "plain"
    And we sleep 200ms
    Then PostgreSQL log should not contain "pgdoorman_prewarm_marker"

  Scenario: a backend whose prewarm_query fails is rejected from the pool
    # The pool referenced by prewarm_bad runs `SELECT * FROM <missing table>`
    # on every fresh backend. The client connection itself must fail because
    # the backend never reaches the idle queue.
    Then psql connection to pg_doorman as user "example_user_1" to database "prewarm_bad" with password "" fails
    # And the per-pool `total_prewarm_failures` counter must surface the
    # failure - a regression that ate the error inside `run_prewarm_query`
    # without bumping the stat would still make the client connection fail
    # (the backend is marked bad either way) but leave operators blind to
    # *why* connections fail. Filter to the affected pool row so a coincident
    # success on another pool doesn't mask the assertion.
    When we create admin session "adm" to pg_doorman as "admin" with password "admin"
    And we execute "SHOW STATS" on admin session "adm" and store response
    Then admin session "adm" column "total_prewarm_failures" for row with "database" = "prewarm_bad" should be between 1 and 9999999
