@rust @rust-3 @discard-all-filter
Feature: DISCARD ALL fast-path in transaction pooling
  In transaction pooling a standalone DISCARD ALL outside of an open
  transaction is functionally a no-op from the application's point of view:
  the next checkout sees a fresh session anyway. Forwarding it to PostgreSQL
  would also clear the per-backend prepared-statement cache, defeating
  transaction-pooling locality. pg_doorman should synthesise the response
  locally and skip the backend round-trip.

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

      [pools.discard_tx]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""

      [[pools.discard_tx.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.discard_session]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "session"
      release_query = ""

      [[pools.discard_session.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.discard_forward]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""
      intercept_discard_all = false

      [[pools.discard_forward.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      """

  Scenario: transaction-mode DISCARD ALL is intercepted (not forwarded)
    When we create session "tx" to pg_doorman as "example_user_1" with password "" and database "discard_tx"
    And we send SimpleQuery "SELECT 1" to session "tx"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send SimpleQuery "DISCARD ALL" to session "tx"
    And we sleep 300ms
    # Backend must NOT see the DISCARD ALL - pg_doorman synthesised the reply.
    Then PostgreSQL log should not contain "DISCARD ALL"

  Scenario: transaction-mode DISCARD ALL with whitespace and semicolon is intercepted
    When we create session "txws" to pg_doorman as "example_user_1" with password "" and database "discard_tx"
    And we send SimpleQuery "SELECT 1" to session "txws"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send SimpleQuery "  discard   all ; " to session "txws"
    And we sleep 300ms
    Then PostgreSQL log should not contain "discard"

  Scenario: session-mode DISCARD ALL IS forwarded to the backend
    # In session pooling the client owns the backend and the cleanup must
    # actually reach PostgreSQL to clear per-connection state.
    When we create session "sess" to pg_doorman as "example_user_1" with password "" and database "discard_session"
    And we send SimpleQuery "SELECT 1" to session "sess"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send SimpleQuery "DISCARD ALL" to session "sess"
    And we sleep 300ms
    Then PostgreSQL log should contain "DISCARD ALL"

  Scenario: multi-statement query containing DISCARD ALL is NOT intercepted
    # Parser rejects multi-statement queries to avoid silently dropping the
    # second statement; the whole batch must reach the backend.
    When we create session "multi" to pg_doorman as "example_user_1" with password "" and database "discard_tx"
    And we send SimpleQuery "SELECT 1" to session "multi"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send SimpleQuery "DISCARD ALL; SELECT 42" to session "multi"
    And we sleep 300ms
    # Backend MUST see the query because we did not intercept it.
    Then PostgreSQL log should contain "SELECT 42"

  Scenario: DISCARD PLANS is NOT intercepted
    # Only standalone DISCARD ALL is intercepted; narrower variants must
    # reach the backend so PostgreSQL's semantics apply.
    When we create session "plans" to pg_doorman as "example_user_1" with password "" and database "discard_tx"
    And we send SimpleQuery "SELECT 1" to session "plans"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send SimpleQuery "DISCARD PLANS" to session "plans"
    And we sleep 300ms
    Then PostgreSQL log should contain "DISCARD PLANS"

  Scenario: intercept_discard_all = false forwards DISCARD ALL to the backend
    # Opt-out pool: applications that rely on real DISCARD ALL semantics
    # (UNLISTEN, ON COMMIT DROP temp tables, two-phase commits) configure
    # intercept_discard_all = false. The query must reach PostgreSQL like
    # any other simple query.
    When we create session "forward" to pg_doorman as "example_user_1" with password "" and database "discard_forward"
    And we send SimpleQuery "SELECT 1" to session "forward"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send SimpleQuery "DISCARD ALL" to session "forward"
    And we sleep 300ms
    Then PostgreSQL log should contain "DISCARD ALL"

  Scenario: extended-protocol DISCARD ALL is rewritten to a no-op
    # Extended-protocol form: client does `Parse("", "DISCARD ALL") + Bind +
    # Execute + Sync`. pg_doorman cannot synthesise a full
    # `ParseComplete + BindComplete + CommandComplete + ReadyForQuery`
    # response without re-implementing PostgreSQL's response-ordering
    # state machine - so instead it rewrites the Parse query text to a
    # zero-parameter no-op (`SELECT 1`) and forwards the rest of the
    # message stream to the backend unchanged. The backend executes
    # SELECT 1, the client gets a valid `ParseComplete + BindComplete +
    # RowDescription + DataRow + CommandComplete + ReadyForQuery` flow,
    # and crucially the backend's prepared-statement cache + planner
    # state are NOT cleared (the entire iServ contract).
    When we create session "ext" to pg_doorman as "example_user_1" with password "" and database "discard_tx"
    And we send SimpleQuery "SELECT 1" to session "ext"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send Parse "" with query "DISCARD ALL" to session "ext"
    And we send Sync to session "ext"
    And we send Bind "" to "" with params "" to session "ext"
    And we send Execute "" to session "ext"
    And we send Sync to session "ext"
    And we sleep 300ms
    # Backend must NOT see `DISCARD ALL` - pg_doorman rewrote the Parse.
    Then PostgreSQL log should not contain "DISCARD ALL"
    # Backend SHOULD see `SELECT 1` (the no-op substitute the rewrite
    # installed). Two unrelated SELECT 1 from elsewhere are filtered out
    # by the `truncate PostgreSQL log` step just above the Parse.
    And PostgreSQL log should contain "SELECT 1"
    # And the per-pool intercept counter must have advanced. A flat value
    # while extended-protocol DISCARD ALL traffic flows is the canonical
    # signature of a future regression in the Parse-rewrite gate.
    When we create admin session "adm" to pg_doorman as "admin" with password "admin"
    And we execute "SHOW STATS" on admin session "adm" and store response
    Then admin session "adm" column "total_discard_all_intercepted" for row with "database" = "discard_tx" should be between 1 and 9999999

  Scenario: cached extended-protocol DISCARD ALL is rejected after transaction start
    # A named Parse can be rewritten and cached while the backend is idle. If
    # the same client later starts a transaction and reuses that cached
    # statement, pg_doorman must not let the cached SELECT 1 substitute run
    # where PostgreSQL would reject DISCARD ALL with active_sql_transaction.
    When we create session "exttx" to pg_doorman as "example_user_1" with password "" and database "discard_tx"
    And we send Parse "ds" with query "DISCARD ALL" to session "exttx"
    And we send Sync to session "exttx"
    And we send SimpleQuery "BEGIN" to session "exttx"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send Bind "" to "ds" with params "" to session "exttx"
    Then session "exttx" should receive ErrorResponse with SQLSTATE "25001"
    And PostgreSQL log should not contain "SELECT 1"

  Scenario: extended-protocol DISCARD ALL on intercept_discard_all=false pool is forwarded verbatim
    # Mirror of the simple-query opt-out scenario. With the per-pool switch
    # off, pg_doorman MUST NOT rewrite the Parse - the backend executes
    # a real DISCARD ALL and the operator's UNLISTEN / ON COMMIT DROP /
    # PREPARE TRANSACTION semantics survive.
    When we create session "extfwd" to pg_doorman as "example_user_1" with password "" and database "discard_forward"
    And we send SimpleQuery "SELECT 1" to session "extfwd"
    And we sleep 200ms
    And we truncate PostgreSQL log
    And we send Parse "" with query "DISCARD ALL" to session "extfwd"
    And we send Sync to session "extfwd"
    And we send Bind "" to "" with params "" to session "extfwd"
    And we send Execute "" to session "extfwd"
    And we send Sync to session "extfwd"
    And we sleep 300ms
    Then PostgreSQL log should contain "DISCARD ALL"

  # ---------------------------------------------------------------------------
  # Driver-level smoke tests. Each spawns the real psycopg2 / asyncpg / npgsql
  # client against the running pg_doorman + PostgreSQL, exercising the actual
  # protocol path the production services use. These are guarded with `@driver`
  # so they can be skipped in CI environments missing the runtime
  # (`--tags 'not @driver'`).
  # ---------------------------------------------------------------------------

  @driver @driver-python
  Scenario: Python drivers (psycopg2 + asyncpg) work through DISCARD ALL intercept
    # Covers both simple-query (psycopg2) and extended-protocol (asyncpg) paths
    # in one script. The script asserts: (a) DISCARD ALL does not raise,
    # (b) the session is usable afterwards, (c) for asyncpg specifically,
    # a server-side prepared statement created BEFORE the DISCARD ALL is
    # still usable AFTER it - direct proof that pg_doorman's Parse rewrite
    # preserved the backend's prepared-statement cache.
    When I run shell command:
      """
      DATABASE_URL="postgresql://example_user_1@127.0.0.1:${DOORMAN_PORT}/discard_tx" \
      EXPECT_INTERCEPT=1 \
      python3 tests/python/test_discard_all_intercept.py
      """
    Then the command should succeed

  @driver @driver-python
  Scenario: Python drivers respect intercept_discard_all=false opt-out
    # When the opt-out is set, real DISCARD ALL reaches the backend and
    # wipes its prepared-statement cache. The asyncpg branch of the test
    # asserts the inverted behaviour: the prepared statement DOES become
    # invalid after DISCARD ALL ("prepared statement does not exist").
    When I run shell command:
      """
      DATABASE_URL="postgresql://example_user_1@127.0.0.1:${DOORMAN_PORT}/discard_forward" \
      EXPECT_INTERCEPT=0 \
      python3 tests/python/test_discard_all_intercept.py
      """
    Then the command should succeed

  @driver @driver-dotnet
  Scenario: Npgsql (.NET) works through DISCARD ALL intercept
    # Covers npgsql's mixed simple-query / extended-protocol path. The
    # script runs DISCARD ALL on both an unprepared and a prepared command,
    # asserts neither throws and that the session is usable afterwards.
    # The .NET runtime is heavy; tests/dotnet/run_test.sh is a thin wrapper
    # that creates a temp dotnet project, adds Npgsql, and runs the .cs file.
    When I run shell command:
      """
      DATABASE_URL="Host=127.0.0.1;Port=${DOORMAN_PORT};Database=discard_tx;Username=example_user_1;Password=;Pooling=false;Include Error Detail=true" \
      bash tests/dotnet/run_test.sh discard_all_intercept discard_all_intercept.cs
      """
    Then the command should succeed
