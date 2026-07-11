@rust @rust-3 @release-query
Feature: Release query after a client RST during response delivery
  A client that resets the TCP connection while pg_doorman is writing a
  fully-buffered backend response must not leak session-local state
  (advisory locks) into the idle pool. Either the release query runs on the
  surviving backend, or the backend is closed and never handed to another
  client.

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
      prepared_statements = true
      pg_hba.content = "host all all 127.0.0.1/32 trust"

      [pools.release_rst]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = "SELECT pg_advisory_unlock_all()"

      [[pools.release_rst.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  Scenario: advisory locks do not leak after a client RST during response delivery
    When we create admin session "admin" to pg_doorman as "admin" with password "admin"
    And we create session "victim" to pg_doorman as "example_user_1" with password "" and database "release_rst"
    # Warm the server-side prepared statement cache with the same SQL, so the
    # flood batches below hit the cache: their Parse is skipped, fast-release
    # is disabled, and no dirty flag is raised on the backend. SQL PREPARE
    # must not be used here - it would arm needs_cleanup_prepare and mask the
    # clean-looking-backend state under test.
    And we send Parse "warm_batch" with query "SELECT pg_advisory_lock(64091), repeat('x', 7000)" to session "victim"
    And we send Bind "" to "warm_batch" with params "" to session "victim"
    And we send Execute "" to session "victim"
    And we send Sync to session "victim"
    # Shrink the client receive buffer, then flood without reading responses.
    # Each response (one ~7KB DataRow plus ReadyForQuery) stays under 8 KiB, so
    # pg_doorman reads it from the backend in one chunk and the backend parks at
    # ReadyForQuery holding the advisory lock. Shrinking only the client receive
    # buffer is not enough on its own: pg_doorman's client-socket send buffer
    # (SO_SNDBUF) autotunes up to net.ipv4.tcp_wmem max (4 MiB on the CI VM) and
    # silently absorbs early responses, so pg_doorman keeps draining the backend
    # and releases it clean (running the release query) before any wedge forms.
    # The flood must therefore buffer more response bytes than that send buffer
    # can hold: 2000 * ~7 KiB = ~14 MiB >> 4 MiB forces pg_doorman's write to the
    # non-reading client to block for real, leaving the backend checked out and
    # parked at ReadyForQuery with the advisory lock still held.
    And we shrink receive buffer to 4096 bytes for session "victim"
    And we send 2000 unread extended query batches "SELECT pg_advisory_lock(64091), repeat('x', 7000)" to session "victim"
    And we wait until the advisory-lock backend is parked in ClientRead and reported active by admin session "admin"
    And we abort TCP connection with RST for session "victim"
    # Either outcome is correct: the release query ran on the surviving
    # backend (locks released), or the backend was closed and the checker
    # gets a fresh one. In both cases the checker's backend must hold no
    # advisory locks.
    When we create session "checker" to pg_doorman as "example_user_1" with password "" and database "release_rst"
    And we send SimpleQuery "SELECT count(*) FROM pg_locks WHERE pid = pg_backend_pid() AND locktype = 'advisory'" to session "checker" and store response
    Then session "checker" should receive DataRow with "0"
