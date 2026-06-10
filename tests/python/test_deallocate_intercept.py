"""
Regression test for simple-query `DEALLOCATE <name>`: pg_doorman must not
answer with a synthetic CommandComplete and
never forwarded it to the backend. In transaction-pool mode the
backend kept the prepared statement, so a subsequent PREPARE of the
same name (possibly on the same backend pulled from the pool, possibly
on a different backend that had served a prior PREPARE with the same
name) failed with SQLSTATE 42P05 `prepared statement "..." already
exists`. Same flavour of state-machine drop as the cancel and GSS
bugs: the legitimate client request was acknowledged as if completed
without actually executing.

Real-client repro: psql `-c PREPARE -c DEALLOCATE -c PREPARE` over the
same DSN, or any application using simple-query `PREPARE/DEALLOCATE`
(psycopg2 default, JDBC `Statement.execute("PREPARE …")`, Django raw
SQL, sqitch deploy scripts).

The two scenarios below cover both failure paths.

Run via:
    DATABASE_URL=postgresql://... python3 test_deallocate_intercept.py
or wired into BDD via `When I run shell command:`.
"""
import os
import subprocess
import sys

DSN = os.getenv("DATABASE_URL", "postgresql://doorman:doorman@127.0.0.1:6433/bench")


def _run_psql_separate_commands(commands):
    """
    Each command becomes a fresh `-c` flag, which in psql means a
    separate simple-query frame on the same DSN. In pg_doorman
    transaction-pool mode each `-c` may land on a different backend
    from the pool - exactly the production pattern that triggers
    42P05 when DEALLOCATE is intercepted.
    """
    args = ["psql", DSN, "-X", "-A", "-t"]
    for c in commands:
        args += ["-c", c]
    env = os.environ.copy()
    return subprocess.run(args, capture_output=True, text=True, env=env)


def test_named_deallocate_allows_reprepare():
    """
    Tier 1: simple-query PREPARE → DEALLOCATE → PREPARE same name
    via three separate `-c` frames. Before fix: 42P05. After fix:
    both PREPAREs succeed.
    """
    name = "f3_named_reprepare_test"
    # Clean any leftover from prior failed runs.
    _run_psql_separate_commands([f"DEALLOCATE {name}"])

    result = _run_psql_separate_commands(
        [
            f"PREPARE {name} AS SELECT 1",
            f"DEALLOCATE {name}",
            f"PREPARE {name} AS SELECT 2",
        ]
    )
    out = (result.stdout + "\n" + result.stderr).strip()
    print("--- named_deallocate_allows_reprepare ---")
    print(f"exit: {result.returncode}")
    print(out)

    # Cleanup before asserting so a failure does not leave a stuck statement.
    _run_psql_separate_commands([f"DEALLOCATE {name}"])

    assert "already exists" not in out, (
        f"PREPARE after DEALLOCATE produced 42P05 - pg_doorman intercepted "
        f"DEALLOCATE without forwarding. Output:\n{out}"
    )
    assert result.returncode == 0, (
        f"psql exited non-zero after the PREPARE/DEALLOCATE/PREPARE sequence. "
        f"Output:\n{out}"
    )


def test_deallocate_all_releases_backend_state():
    """
    Tier 2: DEALLOCATE ALL must wipe the backend's prepared-statement
    list, not only the per-client cache. Before fix: backend retains
    DOORMAN_<n> entries that pg_doorman renamed; a long-lived pool
    eventually accumulates them. The direct symptom that drivers see
    is: PREPARE of a stable name fails with 42P05 if any prior client
    happened to PREPARE the same name on the same backend.
    """
    name = "f3_deallocate_all_test"
    _run_psql_separate_commands([f"DEALLOCATE {name}"])  # cleanup

    result = _run_psql_separate_commands(
        [
            f"PREPARE {name} AS SELECT 1",
            "DEALLOCATE ALL",
            f"PREPARE {name} AS SELECT 2",
        ]
    )
    out = (result.stdout + "\n" + result.stderr).strip()
    print("--- deallocate_all_releases_backend_state ---")
    print(f"exit: {result.returncode}")
    print(out)

    _run_psql_separate_commands([f"DEALLOCATE {name}"])

    assert "already exists" not in out, (
        f"PREPARE after DEALLOCATE ALL produced 42P05 - pg_doorman intercepted "
        f"DEALLOCATE ALL without forwarding; backend still has the name. "
        f"Output:\n{out}"
    )
    assert result.returncode == 0, (
        f"psql exited non-zero after PREPARE/DEALLOCATE ALL/PREPARE. "
        f"Output:\n{out}"
    )


if __name__ == "__main__":
    test_named_deallocate_allows_reprepare()
    test_deallocate_all_releases_backend_state()
    print("OK")
    sys.exit(0)
