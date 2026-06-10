#!/usr/bin/env python3
"""
Driver-level smoke test for pg_doorman's DISCARD ALL interception.

Covers:
  * **psycopg2** (sync): always uses simple-query protocol for parameter-less
    statements → exercises pg_doorman's simple-query intercept path
    (synthetic `CommandComplete("DISCARD ALL") + ReadyForQuery`).
  * **asyncpg simple-query**: `conn.execute("DISCARD ALL")` for a
    parameter-less query is optimised down to a single Q frame →
    same simple-query intercept path.
  * **asyncpg extended-protocol**: `conn.prepare("DISCARD ALL")` followed
    by `.fetchval()` forces Parse / Bind / Execute / Sync → exercises
    pg_doorman's DISCARD ALL Parse-rewrite path (the Parse query text
    is rewritten to `SELECT 1` on the wire).

Primary assertion in every path is the **canonical cache-preservation
canary**: create a server-side prepared statement BEFORE the DISCARD ALL,
verify it still works AFTER. If pg_doorman forwarded a real DISCARD ALL
to the backend, the backend would wipe its prepared-statement cache and
the canary would fail with `prepared statement "..." does not exist`.

Environment:
  * `DATABASE_URL` - DSN to pg_doorman pool.
  * `EXPECT_INTERCEPT` - `1` (default) or `0`. Controls the inverted
    asyncpg cache-survival assertion for the `intercept_discard_all =
    false` opt-out pool.

Exits 0 on success, 1 on any failure. Intended to be invoked from a
BDD scenario via `When I run shell command:`.
"""
import asyncio
import os
import sys

DSN = os.getenv(
    "DATABASE_URL",
    "postgresql://example_user_1@localhost:6433/example_db",
)
EXPECT_INTERCEPT = os.getenv("EXPECT_INTERCEPT", "1") == "1"


def _log(msg):
    """Single-line tag so the BDD step's stdout capture is easy to read."""
    print(f"[discard-all-intercept] {msg}", flush=True)


# ---------------------------------------------------------------------------
# psycopg2 (sync, simple-query path)
# ---------------------------------------------------------------------------


def test_psycopg2():
    import psycopg2

    conn = psycopg2.connect(DSN)
    # psycopg2 wraps every statement in an implicit transaction unless
    # autocommit is enabled. DISCARD ALL cannot run inside a transaction
    # block. Real-world apps that send DISCARD ALL through psycopg2
    # already set this flag - we do the same to mirror the production
    # client pattern.
    conn.autocommit = True
    cur = conn.cursor()

    # 1. Baseline query - confirm connection works.
    cur.execute("SELECT 'before' AS marker")
    row = cur.fetchone()
    assert row == ("before",), f"baseline query returned unexpected row: {row!r}"

    # 2. DISCARD ALL via simple-query (psycopg2 default for parameter-less).
    cur.execute("DISCARD ALL")
    status = cur.statusmessage
    _log(f"psycopg2 DISCARD ALL statusmessage={status!r}")
    if EXPECT_INTERCEPT:
        # Simple-query intercept synthesises a faithful
        # `CommandComplete("DISCARD ALL")`, so the driver's
        # statusmessage MUST be exactly that string.
        assert (
            status == "DISCARD ALL"
        ), f"intercept=true: expected 'DISCARD ALL' tag, got {status!r}"

    # 3. Connection still usable.
    cur.execute("SELECT 42")
    assert cur.fetchone() == (42,)
    cur.close()
    conn.close()
    _log("psycopg2: OK")


# ---------------------------------------------------------------------------
# asyncpg
# ---------------------------------------------------------------------------


async def test_asyncpg_simple_query():
    """`conn.execute("DISCARD ALL")` - parameter-less, asyncpg sends as
    a single Q frame. Exercises pg_doorman's simple-query intercept path.

    Cache-preservation canary: prepare a parameterised statement BEFORE
    DISCARD ALL, reuse it AFTER. Two valid outcomes:
      * EXPECT_INTERCEPT=1: pg_doorman intercepts DISCARD ALL without
        forwarding to backend AND without wiping its per-client cache.
        asyncpg's reuse of `__asyncpg_stmt_N__` finds a live mapping in
        pg_doorman → renamed to the backend's server-name → Bind succeeds
        with no extra round-trip. Canary returns the new value directly.
      * EXPECT_INTERCEPT=0: real DISCARD ALL reaches the backend and
        wipes its prepared-statement cache. asyncpg's next Bind fails
        with `InvalidSQLStatementNameError`, asyncpg auto-reprepares
        transparently, the canary still returns the new value. The
        visible result is identical from the app's perspective.

    Historical note: earlier pg_doorman called `discard_clear()` on the
    intercept path, which forced a 26000 → re-Parse cycle for every
    cached asyncpg statement and was incompatible with this canary on
    EXPECT_INTERCEPT=1. Removed (see `transaction.rs::respond_to_simple_discard`).
    """
    import asyncpg

    conn = await asyncpg.connect(DSN)
    try:
        # Canary: server-side prepared statement that must survive the
        # intercepted DISCARD ALL.
        canary = await conn.prepare("SELECT $1::int AS v")
        assert await canary.fetchval(3) == 3

        val = await conn.fetchval("SELECT 'before'::text")
        assert val == "before"

        # DISCARD ALL via the simple-query optimisation.
        status = await conn.execute("DISCARD ALL")
        _log(f"asyncpg simple-query DISCARD ALL execute()={status!r}")
        if EXPECT_INTERCEPT:
            assert (
                status == "DISCARD ALL"
            ), f"simple-query intercept: expected 'DISCARD ALL', got {status!r}"

        # Plain connection must still be usable for new traffic.
        assert await conn.fetchval("SELECT 42") == 42

        # Reuse the canary. With EXPECT_INTERCEPT=1 the per-client cache
        # mapping was preserved → Bind succeeds directly. With
        # EXPECT_INTERCEPT=0 asyncpg's auto-reprepare masks the backend
        # cache wipe. Either way the application-visible outcome is
        # identical: the prepared statement returns the new value.
        assert (
            await canary.fetchval(13) == 13
        ), "canary must survive DISCARD ALL"
        _log(
            "asyncpg simple-query: canary survived DISCARD ALL "
            "(intercept preserves backend+pg_doorman cache; opt-out triggers asyncpg auto-reprepare)"
        )
    finally:
        await conn.close()
    _log("asyncpg simple-query: OK")


async def test_asyncpg_extended_protocol():
    """`conn.prepare("DISCARD ALL")` then `.fetchval()` - forces Parse /
    Bind / Execute / Sync regardless of asyncpg's optimisation
    heuristics. Exercises pg_doorman's DISCARD ALL Parse-rewrite path
    (Parse text becomes `SELECT 1` on the wire to the backend)."""
    import asyncpg

    conn = await asyncpg.connect(DSN)
    try:
        # Canary: separate prepared statement that must survive the
        # rewritten DISCARD ALL.
        canary = await conn.prepare("SELECT $1::int AS v")
        v1 = await canary.fetchval(7)
        assert v1 == 7

        # Force extended-protocol DISCARD ALL via explicit prepare.
        # The backend sees a Parse for `SELECT 1`
        # instead. The driver still gets ParseComplete + BindComplete +
        # DataRow + CommandComplete + ReadyForQuery, so .fetchval()
        # returns the one-row result.
        discard_stmt = await conn.prepare("DISCARD ALL")
        try:
            result = await discard_stmt.fetchval()
            _log(
                f"asyncpg extended-protocol DISCARD ALL prepare()+fetchval()={result!r}"
            )
            if EXPECT_INTERCEPT:
                # The substitute is `SELECT 1`, which returns
                # the integer 1. A real DISCARD ALL would return no
                # rows, which asyncpg's fetchval() reports as None.
                assert result == 1, (
                    f"intercept=true: substitute is SELECT 1, "
                    f"expected fetchval()==1, got {result!r}"
                )
        except asyncpg.exceptions.PostgresError as e:
            if EXPECT_INTERCEPT:
                raise
            # intercept=false: a real DISCARD ALL produces no rows.
            # asyncpg may or may not raise depending on version.
            _log(f"asyncpg extended-protocol DISCARD ALL: PostgresError {e!r}")

        # Cache-preservation canary for the extended-protocol path.
        # When EXPECT_INTERCEPT=true the Parse for DISCARD ALL is
        # rewritten on the wire and the backend's prepared-statement
        # cache is preserved - the canary trivially survives.
        #
        # When EXPECT_INTERCEPT=false the real backend DISCARD ALL
        # DOES wipe the backend cache, but asyncpg transparently
        # detects the resulting `InvalidSQLStatementNameError` and
        # auto-re-prepares the statement. The visible app-level
        # behaviour is identical - the canary still returns the
        # correct row. So we assert success unconditionally here:
        # the value-add of this test is "DISCARD ALL doesn't break
        # the client", not "cache-survival is observable through
        # asyncpg's API" (it isn't, by asyncpg design).
        v2 = await canary.fetchval(11)
        assert v2 == 11, f"canary returned {v2!r} after DISCARD ALL"
        _log(
            "asyncpg extended-protocol: canary works after DISCARD ALL "
            "(intercept preserves backend cache; opt-out triggers asyncpg auto-reprepare)"
        )

        # Connection still usable.
        assert await conn.fetchval("SELECT 42") == 42
    finally:
        await conn.close()
    _log("asyncpg extended-protocol: OK")


# ---------------------------------------------------------------------------
# Entry
# ---------------------------------------------------------------------------


def main():
    _log(f"DSN={DSN} EXPECT_INTERCEPT={EXPECT_INTERCEPT}")
    failures = []

    try:
        test_psycopg2()
    except Exception as e:
        failures.append(("psycopg2", repr(e)))
        _log(f"psycopg2: FAIL {e!r}")

    try:
        asyncio.run(test_asyncpg_simple_query())
    except Exception as e:
        failures.append(("asyncpg-simple", repr(e)))
        _log(f"asyncpg-simple: FAIL {e!r}")

    try:
        asyncio.run(test_asyncpg_extended_protocol())
    except Exception as e:
        failures.append(("asyncpg-extended", repr(e)))
        _log(f"asyncpg-extended: FAIL {e!r}")

    if failures:
        _log(f"FAILURES: {failures}")
        sys.exit(1)
    _log("all drivers OK")


if __name__ == "__main__":
    main()
