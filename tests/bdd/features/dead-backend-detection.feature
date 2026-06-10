@rust @rust-3 @dead-backend-detection
Feature: Pool recovers after PostgreSQL restart via dead-backend liveness scan
  The iServ "zombie pool" fix scans idle backends in `retain_connections`
  and drops the ones whose `check_alive` fails. Without it, after a PostgreSQL
  restart `slots.size` keeps counting stale TCP sockets until `idle_timeout`
  expires, and `replenish` never refills the pool. The scenarios below
  exercise that situation:
   * the restart scenario asserts subsequent client queries succeed AND
     that the per-pool `total_dead_backends_evicted` counter advanced
     past zero - direct proof that `evict_dead_backends` actually ran;
   * the column-presence scenario verifies the new `SHOW STATS` columns
     are wired up even on a fully healthy pool, so dashboards do not
     silently break.

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
      retain_connections_time = "2s"
      dead_backend_check_timeout = "1s"
      dead_backend_check_max_per_cycle = 8
      pg_hba.content = "host all all 127.0.0.1/32 trust"

      [pools.dead_tx]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""

      [[pools.dead_tx.users]]
      username = "example_user_1"
      password = ""
      pool_size = 4
      min_pool_size = 2
      """

  Scenario: client queries continue to succeed after PostgreSQL restart
    # Force at least one backend creation by issuing a real query.
    When we create session "pre" to pg_doorman as "example_user_1" with password "" and database "dead_tx"
    And we send SimpleQuery "SELECT 1" to session "pre"
    And we sleep 300ms
    And we close session "pre"
    # Trigger the regression condition.
    When PostgreSQL is restarted
    # Give retain loop multiple ticks to (a) detect dead backends via
    # check_alive, (b) tear them down, (c) let replenish recreate fresh
    # ones. With retain_connections_time=2s and dead_check_max_per_cycle=8
    # this finishes well within 8s on a healthy host; we sleep 10s to be
    # generous for VMs under load.
    And we sleep 10000ms
    # Direct verification that `evict_dead_backends` actually fired and
    # ejected the post-restart zombies: the per-pool counter must have
    # advanced past zero. A regression that recovered via client-side
    # reconnect / fresh checkout without ever running the eviction scan
    # would leave this counter flat.
    When we create admin session "adm" to pg_doorman as "admin" with password "admin"
    And we execute "SHOW STATS" on admin session "adm" and store response
    Then admin session "adm" column "total_dead_backends_evicted" should be at least 1
    # And a brand-new client session must work end-to-end: this proves
    # the pool is functional after the eviction + replenish cycle, not
    # stuck behind zombie sockets. The counter assertion above checks the
    # eviction directly; the post-restart usability assertion below only
    # requires the query to come back without an error.
    When we create session "post" to pg_doorman as "example_user_1" with password "" and database "dead_tx"
    And we send SimpleQuery "SELECT 42" to session "post"

  Scenario: SHOW STATS exposes the new dead-backend counters
    # No restart required for this column-presence check - the feature only
    # needs `evict_dead_backends` wired into the stats path. A regression
    # that dropped the column would surface here even on a fully healthy pool.
    When we create admin session "adm" to pg_doorman as "admin" with password "admin"
    And we execute "SHOW STATS" on admin session "adm" and store response
    Then admin session "adm" response should contain "total_dead_backends_probed"
    And admin session "adm" response should contain "total_dead_backends_evicted"
    And admin session "adm" response should contain "total_prewarm_failures"
