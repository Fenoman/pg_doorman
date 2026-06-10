@rust @rust-4 @setapp-piggyback
Feature: Piggyback SET application_name on the simple-query first message
  with sync_server_parameters = true and an app_name-only diff at
  checkout, pg_doorman defers the `SET application_name` and concatenates it as
  a separate `Q` frame ahead of the client's FIRST simple-query in a single
  flush, swallowing exactly the SET's CommandComplete + ReadyForQuery before
  relaying the client's own response. The piggybacked SET MUST take effect
  BEFORE the client's query runs, so a fresh client reusing a warm backend
  sees its OWN application_name via current_setting() - with correct result
  rows and no driver/protocol error.

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
      # sync_server_parameters is a [general]-only setting (config.general):
      # PoolSettings.sync_server_parameters is always populated from
      # config.general (pool/mod.rs:851/1087, dynamic.rs:281), never from a
      # per-pool key. Placing it under [pools.example_db] left it at its
      # default `false`, so the whole checkout-sync path was skipped and the
      # piggyback never fired (svc-b observed the pooler's own 'pg_doorman').
      # It MUST live in [general] - same as sync-parameters-quoting.feature and
      # prepared-cache-startup-parameters.feature.
      sync_server_parameters = true

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  @setapp-piggyback-simple-query
  Scenario: Piggybacked SET is visible to the very first query on a warm backend
    # Warm the single pooled backend under svc-a's application_name.
    When I open session "a" with application_name "svc-a"
    And session "a" runs simple query "SELECT 1"
    Then session "a" last query returned "1"
    # A fresh client (svc-b) reuses that same warm backend (pool_size = 1).
    # Its FIRST simple-query carries the deferred SET application_name. The
    # query must observe its OWN app_name, proving the SET applied BEFORE it.
    When I open session "b" with application_name "svc-b"
    And session "b" runs simple query "SELECT current_setting('application_name')"
    Then session "b" last query returned "svc-b"
    # And the canonical SHOW path agrees, with no protocol error in between.
    And session "b" sees application_name "svc-b"
    # svc-a's later checkout must still observe svc-a (no leak from svc-b).
    And session "a" sees application_name "svc-a"

  @setapp-piggyback-warm-pool-own-name
  Scenario: A fresh client entering a warm pool sees its own application_name
    When I open session "a" with application_name "svc-a"
    And session "a" runs simple query "SELECT 1"
    When I open session "warm" with application_name "svc-warm"
    And session "warm" runs simple query "SELECT current_setting('application_name')"
    Then session "warm" last query returned "svc-warm"

  @setapp-piggyback-deferred-begin-rollback
  Scenario: A deferred SET survives a client ROLLBACK (SET is flushed before the deferred BEGIN)
    # Deferred-BEGIN ordering. application_name is a non-LOCAL GUC:
    # if the deferred SET were piggybacked onto the first query INSIDE the
    # client's transaction, a ROLLBACK would revert it and the reused backend
    # would keep advertising the previous service's name. pg_doorman flushes the
    # deferred SET BEFORE the deferred BEGIN (outside any transaction), so the
    # SET survives the ROLLBACK.
    When I open session "a" with application_name "svc-a"
    And session "a" runs simple query "SELECT 1"
    Then session "a" last query returned "1"
    # svc-b reuses the warm backend (pool_size = 1). Its FIRST client message is
    # a standalone BEGIN (a deferred-BEGIN checkout). The deferred SET
    # application_name must be flushed BEFORE that BEGIN.
    When I open session "b" with application_name "svc-b"
    And session "b" runs simple query "BEGIN"
    # Inside the transaction, the app_name is already svc-b (SET applied before BEGIN).
    And session "b" runs simple query "SELECT current_setting('application_name')"
    Then session "b" last query returned "svc-b"
    # ROLLBACK the transaction. A non-LOCAL SET issued OUTSIDE the transaction
    # (as pg_doorman does) is NOT reverted by this ROLLBACK.
    When session "b" runs simple query "ROLLBACK"
    # After the rollback, svc-b's app_name is still svc-b - proving the SET ran
    # outside the transaction. (On the buggy ordering it would read svc-a here.)
    And session "b" runs simple query "SELECT current_setting('application_name')"
    Then session "b" last query returned "svc-b"
    And session "b" sees application_name "svc-b"
    # svc-a's later checkout still observes svc-a (no cross-client leak).
    And session "a" sees application_name "svc-a"

  @setapp-piggyback-client-set-in-tx-commit
  Scenario: A client's own SET application_name inside its transaction is committed and persists
    # The service itself runs `BEGIN; SET application_name = '...'; ...; COMMIT`.
    # A standalone BEGIN is deferred by the pooler (checkout happens on the next
    # message), so the pooler applies the client's OWN startup name (svc-c)
    # OUTSIDE the transaction at checkout, then the client's in-transaction SET
    # rides inside the transaction exactly as the client intends.
    When I open session "a" with application_name "svc-a"
    And session "a" runs simple query "SELECT 1"
    Then session "a" last query returned "1"
    # Client "c" reuses the warm backend (pool_size = 1).
    When I open session "c" with application_name "svc-c"
    And session "c" runs simple query "BEGIN"
    And session "c" runs simple query "SET application_name = 'inflight'"
    And session "c" runs simple query "SELECT current_setting('application_name')"
    # Inside the transaction the client's own SET is in effect.
    Then session "c" last query returned "inflight"
    # Explicit keywords matter: `runs simple query` is a #[when] step,
    # `sees application_name` is a #[then] step. A bare `And` inherits the
    # previous primary keyword, so a family switch (When->Then or Then->When)
    # must restate the keyword or cucumber looks the step up in the wrong table
    # and skips it.
    When session "c" runs simple query "COMMIT"
    # The client COMMITted its own `SET application_name = 'inflight'`, so the
    # value is durable for THIS client's subsequent queries on the backend it is
    # actively using - current_setting() returns 'inflight'. This is correct and
    # not a leak: 'inflight' is the client's OWN committed value, never another
    # service's name. (Checkin RESET ALL only fires when the backend is released
    # back to the pool, which has not happened while c keeps issuing queries.)
    Then session "c" sees application_name "inflight"
    # svc-a is on its own pooled checkout and is unaffected - no cross-client
    # leak from c's committed SET.
    And session "a" sees application_name "svc-a"

  @setapp-piggyback-client-set-in-tx-rollback
  Scenario: A client's own SET application_name is reverted by ROLLBACK to the client's startup name
    When I open session "a" with application_name "svc-a"
    And session "a" runs simple query "SELECT 1"
    When I open session "c" with application_name "svc-c"
    And session "c" runs simple query "BEGIN"
    And session "c" runs simple query "SET application_name = 'inflight'"
    And session "c" runs simple query "SELECT current_setting('application_name')"
    Then session "c" last query returned "inflight"
    When session "c" runs simple query "ROLLBACK"
    # ROLLBACK reverts the in-transaction SET. Because the pooler applied the
    # client's OWN startup name (svc-c) OUTSIDE the transaction at checkout (the
    # deferred-SET-before-deferred-BEGIN fix), the revert target is svc-c - the
    # client's own name, NOT a previous service's. No audit leak.
    And session "c" runs simple query "SELECT current_setting('application_name')"
    Then session "c" last query returned "svc-c"
    And session "c" sees application_name "svc-c"
    And session "a" sees application_name "svc-a"

  @setapp-piggyback-discard-all-clears-pending-set
  Scenario: DISCARD ALL as the first query is intercepted and drops the deferred SET
    # A standalone DISCARD ALL must never reach the backend in
    # transaction mode (it would wipe the prepared-statement cache and shared
    # per-backend state). When the checkout deferred a SET application_name,
    # the interception fires first and the deferred SET is dropped - it is
    # bound to this checkout and must not leak into the next one. The next real
    # query re-checks out, re-diffs and re-defers the SET correctly, so the
    # client still observes its OWN application_name (and svc-a is unaffected).
    When I open session "a" with application_name "svc-a"
    And session "a" runs simple query "SELECT 1"
    # svc-b reuses the warm backend; its FIRST simple-query is DISCARD ALL,
    # which is intercepted (synthesised response, no backend round-trip) and
    # must clear svc-b's deferred SET.
    When I open session "b" with application_name "svc-b"
    And session "b" runs simple query "DISCARD ALL"
    # svc-b's next query re-defers + applies its own SET - no stale leak, no
    # protocol error from a double SET.
    And session "b" runs simple query "SELECT current_setting('application_name')"
    Then session "b" last query returned "svc-b"
    And session "b" sees application_name "svc-b"
    # svc-a's later checkout still observes svc-a (no cross-client leak).
    And session "a" sees application_name "svc-a"
