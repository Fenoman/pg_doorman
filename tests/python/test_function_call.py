"""
Regression test for PostgreSQL FunctionCall passthrough: pg_doorman must not
drop the FunctionCall message (`'F'`) with `ProtocolSyncError` or
marked the backend bad, breaking every libpq application using the
large-object (`lo_*`) API via `PQfn`.

The `'F'` frame is legacy but still in the PG protocol spec (see
https://www.postgresql.org/docs/current/protocol-message-formats.html
- FunctionCall(F)/FunctionCallResponse(V)). libpq's `lo_creat`,
`lo_open`, `lo_read`, `lo_write`, `lo_close`, `lo_unlink` all dispatch
via `PQfn`, so blobs through psycopg2's `lobject()`, PHP's `pg_lo_*`,
Perl DBD::Pg's `lo_*`, and `pg_dump --large-objects` all break in
pg_doorman in front of any backend.

Before fix: psycopg2 `conn.lobject(0, "w")` →
`OperationalError: server closed the connection unexpectedly`.
After fix: large object created, written, read back, and unlinked
cleanly.

Real-client repro covered: psycopg2 ≥ 2.9 `lobject()`; same code path
in any libpq-based client by symmetry.

Run via:
    DATABASE_URL=postgresql://... python3 test_function_call.py
or wired into BDD via `When I run shell command:`.
"""
import os
import sys

import psycopg2

DSN = os.getenv("DATABASE_URL", "postgresql://doorman:doorman@127.0.0.1:6433/bench")


def test_lobject_round_trip_via_function_call():
    """
    psycopg2 `lobject()` dispatches through libpq `PQfn` which emits
    the PG protocol `'F'` (FunctionCall) frame. pg_doorman must forward
    the frame to the backend and relay the `'V'` (FunctionCallResponse)
    + `'Z'` (ReadyForQuery) reply back to the client.

    The whole round-trip MUST happen inside an open transaction -
    PostgreSQL refuses LO operations otherwise (`large object
    descriptor 0 was not opened for writing`).
    """
    payload = b"pg_doorman F4 regression payload"

    conn = psycopg2.connect(DSN)
    conn.autocommit = False
    try:
        # 1. Create the large object. `0` is the OID hint meaning
        # "let the server pick a free one". `'w'` opens for writing.
        lo = conn.lobject(0, "w")
        oid = lo.oid
        assert oid > 0, f"expected a valid OID, got {oid!r}"
        lo.write(payload)
        lo.close()
        conn.commit()

        # 2. Read back in a fresh transaction (also via PQfn).
        # psycopg2 lobject mode 'rb' returns bytes (vs 'r' which decodes).
        lo2 = conn.lobject(oid, "rb")
        try:
            data = lo2.read()
        finally:
            lo2.close()
        conn.commit()
        assert data == payload, f"round-tripped data mismatch: {data!r}"

        # 3. Unlink - also a PQfn call (`lo_unlink`).
        conn.lobject(oid, "n", 0).unlink()
        conn.commit()
    except psycopg2.OperationalError as e:
        raise AssertionError(
            f"pg_doorman dropped the FunctionCall frame ('F') - libpq "
            f"reported {e!r}. Forward the 'F' frame to the backend "
            f"(see src/client/transaction.rs message dispatch)."
        ) from e
    finally:
        conn.close()


if __name__ == "__main__":
    test_lobject_round_trip_via_function_call()
    print("OK")
    sys.exit(0)
