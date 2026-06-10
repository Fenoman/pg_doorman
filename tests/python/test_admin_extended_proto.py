"""
Regression test for admin extended-protocol errors: pg_doorman's admin
handler must not return `Err(ProtocolSyncError(...))` on non-`'Q'` client
message code, and the error never reached the wire (admin path skips
`process_error`). The socket was dropped silently, so drivers that
default to the extended-query protocol - psycopg3, asyncpg, pgjdbc
with `simpleProtocolOnly=false`, npgsql - surfaced
`OperationalError: server closed the connection unexpectedly` on
simple admin commands like `SHOW POOLS`.

After fix: pg_doorman replies with a typed ErrorResponse
(SQLSTATE 0A000 - feature_not_supported) and keeps the socket alive.

We exercise the wire directly with a raw socket and the libpq SCRAM
auth implementation from psycopg3, so we can guarantee the Parse
frame actually goes out (psycopg3's higher-level `execute()` may
auto-route a parameter-less statement through simple-query). The
ErrorResponse must arrive without EOF.
"""
import os
import socket
import struct
import sys

import psycopg

ADMIN_DSN = os.getenv(
    "ADMIN_DSN", "postgresql://admin:admin@127.0.0.1:6433/pgbouncer"
)


def _build_parse_sync(query: str) -> bytes:
    """
    Two pipelined frames: empty-name Parse(SHOW POOLS) and Sync.
    Parse:  'P' + len + name\\0 + query\\0 + i16(num_param_types=0)
    Sync:   'S' + len(4)
    """
    name = b"\0"
    q = query.encode("utf-8") + b"\0"
    num_params = struct.pack("!h", 0)
    parse_body = name + q + num_params
    parse_len = struct.pack("!i", 4 + len(parse_body))
    parse_frame = b"P" + parse_len + parse_body
    sync_frame = b"S" + struct.pack("!i", 4)
    return parse_frame + sync_frame


def _read_admin_extended_reply(conn: psycopg.Connection, query: str) -> tuple[bool, str]:
    """
    Drop into the raw fd under the psycopg connection, send Parse+Sync,
    read until ReadyForQuery('Z') or EOF. Returns (got_error_response, payload).
    """
    fd = conn.fileno()
    sock = socket.socket(fileno=os.dup(fd))
    try:
        sock.setblocking(True)
        sock.sendall(_build_parse_sync(query))

        got_error = False
        got_ready = False
        last_error_text = ""
        buf = b""
        while not got_ready:
            chunk = sock.recv(4096)
            if not chunk:
                return (False, "EOF - server dropped the socket on Parse frame")
            buf += chunk
            # Naive framer: walk type+len, consume body.
            while len(buf) >= 5:
                ty = buf[0:1]
                (length,) = struct.unpack("!i", buf[1:5])
                if len(buf) < 1 + length:
                    break  # need more bytes
                body = buf[5 : 1 + length]
                buf = buf[1 + length :]
                if ty == b"E":
                    got_error = True
                    # ErrorResponse fields are 1-byte tag + cstring blocks.
                    last_error_text = body.replace(b"\0", b" ").decode(
                        "utf-8", errors="replace"
                    )
                elif ty == b"Z":
                    got_ready = True
                    break
        return (got_error, last_error_text)
    finally:
        sock.close()


def test_admin_extended_protocol_returns_error_not_disconnect():
    conn = psycopg.connect(ADMIN_DSN, autocommit=True)
    try:
        got_error, payload = _read_admin_extended_reply(conn, "SHOW POOLS")
    finally:
        conn.close()

    print(f"got_error={got_error} payload={payload!r}")

    assert "EOF" not in payload, (
        f"pg_doorman dropped the socket on extended Parse - F12 regressed.\n"
        f"payload: {payload}"
    )
    assert got_error, (
        "expected an ErrorResponse (typed 0A000) on extended Parse against admin db; "
        f"got payload: {payload}"
    )
    # Friendly check that pg_doorman shaped the message intentionally,
    # not an accidental generic error.
    assert "extended" in payload.lower() or "0A000" in payload or "not supported" in payload.lower(), (
        f"ErrorResponse should explain the extended-protocol limitation; got: {payload}"
    )


if __name__ == "__main__":
    test_admin_extended_protocol_returns_error_not_disconnect()
    print("OK")
    sys.exit(0)
