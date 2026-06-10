"""
Regression test for psycopg3 cancel_safe() through pg_doorman.

Background:
  psycopg3's `Connection.cancel_safe()` uses libpq's `PQcancelConn` (libpq 17+).
  Unlike the legacy `cancel()` (which calls `PQcancel` and sends a raw 16-byte
  CancelRequest), `PQcancelConn` honours `sslmode` on the cancel socket. With
  the default `sslmode=prefer` it first sends an `SSLRequest`, accepts the
  `'N'` rejection when TLS is not configured, then sends the `CancelRequest`
  over the same plain socket - a legitimate path per the PostgreSQL protocol.

  pg_doorman <= local3.6.2 (and unfixed local3.10.6) dropped that CancelRequest
  with `ProtocolSyncError("Unexpected protocol message during plain-text startup
  negotiation")`. libpq surfaces it as:

      cancellation failed: connection to server at "...", port ... failed:
      server closed the connection unexpectedly

  The fix routes post-SSL-rejected CancelRequest the same way the direct-cancel
  and post-TLS-cancel paths route - see `src/client/entrypoint.rs`.

This file launches each scenario in a fresh Python interpreter so that libpq's
C-level stderr writes are captured by the parent process for assertions -
identical pattern to `test_cancel_query.py`.

Requires `psycopg[binary] >= 3.2.0` (cancel_safe is in 3.1.5+; 3.2+ has the
production-grade implementation we test against).
"""
import os
import subprocess
import sys
from textwrap import dedent

import pytest

DEFAULT_DB_URL = "postgresql://example_user_1:test@127.0.0.1:6433/example_db"


def _run_cancel_safe_scenario(sslmode: str, cancel_method: str):
    """
    Run a long query and cancel it via the chosen psycopg3 API.

    `cancel_method` is either "cancel" (legacy PQcancel) or
    "cancel_safe" (PQcancelConn, libpq 17+).

    Subprocess so we capture real fd 2 from libpq - the noisy
    "server closed the connection unexpectedly" message is written
    from C, never reaches sys.stderr.
    """
    db_url = os.getenv("DATABASE_URL", DEFAULT_DB_URL)

    script = dedent(
        rf"""
        import os
        import sys
        import threading
        import time

        import psycopg

        db_url = os.getenv("DATABASE_URL", {db_url!r})
        # Override sslmode via DSN param so the parent test controls it.
        sep = "&" if "?" in db_url else "?"
        if "sslmode=" in db_url:
            import re
            db_url = re.sub(r"sslmode=[^&]+", "sslmode={sslmode}", db_url)
        else:
            db_url = db_url + sep + "sslmode={sslmode}"

        conn = psycopg.connect(db_url, autocommit=True)

        holder = {{"err": None}}
        def run_query():
            try:
                with conn.cursor() as cur:
                    cur.execute("SELECT pg_sleep(10)")
            except Exception as e:
                holder["err"] = e

        t = threading.Thread(target=run_query, daemon=True)
        t.start()
        time.sleep(1.0)

        cancel_call_err = None
        try:
            if "{cancel_method}" == "cancel":
                conn.cancel()
            else:
                conn.cancel_safe(timeout=5.0)
        except Exception as e:
            cancel_call_err = repr(e)

        t.join(timeout=12.0)

        try:
            conn.close()
        except Exception:
            pass

        # Machine-readable status the parent test parses.
        print("CANCEL_CALL_ERR:", cancel_call_err or "None")
        print("QUERY_ERR:", repr(holder["err"]) if holder["err"] else "None")
        print(
            "QUERY_CANCELED:",
            isinstance(holder["err"], psycopg.errors.QueryCanceled),
        )
        print("done test")
        """
    )

    completed = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        check=False,
        env=os.environ.copy(),
    )
    return completed.returncode, completed.stdout, completed.stderr


def _assert_clean_cancel(code, out, err, *, expect_query_canceled: bool):
    """Common assertions shared by all scenarios."""
    print(f"--- subprocess exit={code} ---")
    print(f"--- stdout ---\n{out}")
    print(f"--- stderr ---\n{err}")

    assert code == 0, (
        f"Subprocess exited non-zero ({code}); "
        f"this means the cancel scenario crashed.\nstderr: {err}"
    )

    noise_markers = [
        "server closed the connection unexpectedly",
        "cancellation failed: connection to server",
    ]
    for marker in noise_markers:
        assert marker not in err, (
            f"libpq printed noise marker {marker!r} to stderr - pg_doorman dropped "
            f"the cancel socket instead of routing the CancelRequest.\n"
            f"Full stderr:\n{err}"
        )

    assert "CANCEL_CALL_ERR: None" in out, (
        "psycopg's cancel call raised an exception - pg_doorman is rejecting "
        "the CancelRequest at the entrypoint.\n"
        f"stdout:\n{out}"
    )

    if expect_query_canceled:
        assert "QUERY_CANCELED: True" in out, (
            "The long query was NOT terminated by the cancel - cancel reached "
            "pg_doorman but never made it to the backend.\n"
            f"stdout:\n{out}"
        )

    assert "done test" in out, (
        f"Subprocess did not reach end-of-script anchor; truncated output.\n"
        f"stdout:\n{out}"
    )


def test_cancel_safe_with_sslmode_prefer_is_clean():
    """
    Primary regression: psycopg3 cancel_safe() with sslmode=prefer (the default).
    Before the fix this raised
      OperationalError('cancellation failed: ... server closed the connection
      unexpectedly')
    and left the long query running.
    """
    code, out, err = _run_cancel_safe_scenario(
        sslmode="prefer", cancel_method="cancel_safe"
    )
    _assert_clean_cancel(code, out, err, expect_query_canceled=True)


def test_cancel_safe_with_sslmode_disable_is_clean():
    """
    Sanity check: cancel_safe() with sslmode=disable skips SSLRequest entirely
    and hits the direct-cancel arm. Should have always worked, even before
    the fix.
    """
    code, out, err = _run_cancel_safe_scenario(
        sslmode="disable", cancel_method="cancel_safe"
    )
    _assert_clean_cancel(code, out, err, expect_query_canceled=True)


def test_legacy_cancel_is_clean():
    """
    Sanity check: legacy conn.cancel() (PQcancel) never negotiates SSL on the
    cancel socket. Should have always worked.
    """
    code, out, err = _run_cancel_safe_scenario(
        sslmode="prefer", cancel_method="cancel"
    )
    _assert_clean_cancel(code, out, err, expect_query_canceled=True)
