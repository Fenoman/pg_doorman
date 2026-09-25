#!/usr/bin/env bash
#
# Records the PGO profile that release builds of pg_doorman use,
# pgo/pg_doorman.profdata.gz.
#
# Usage:
#   make pgo-profile
#   PG_BIN=/usr/lib/postgresql/17/bin ./scripts/pgo-profile.sh
#
# Steps:
#   1. Builds pg_doorman with -Cprofile-generate into target/pgo-gen.
#   2. Starts a throwaway PostgreSQL cluster in a temporary directory (unix
#      socket in that directory, fsync off) and runs `pgbench -i -s 10`.
#   3. Runs the instrumented pg_doorman in front of it twice, with the
#      default release_query and with release_query = "". Each time pgbench
#      runs select-only with the simple, extended and prepared protocols at
#      8 and 32 clients, then tpcb-like at 32 clients, PGO_DURATION seconds
#      per run, and the TPS of every run is printed.
#   4. Stops pg_doorman with SIGTERM, which writes its raw profile, merges
#      the raw profiles with llvm-profdata and writes the gzipped result.
# pg_doorman and PostgreSQL are stopped and the temporary directory is
# removed on any exit, including errors and Ctrl-C.
#
# Release builds look up profile records by symbol name, and symbol names
# depend on the rustc version, the host and the package version in
# Cargo.toml. Record the committed profile on x86_64 Linux with the
# toolchain from rust-toolchain.toml, after the version bump of a release.
#
# Requirements:
#   - llvm-profdata from the llvm-tools component of the active toolchain:
#       rustup component add llvm-tools --toolchain <rustc version>
#   - initdb, pg_ctl, createdb and pgbench in PG_BIN or PATH
#   - as root, runuser and an unprivileged PG_OS_USER: the PostgreSQL server
#     refuses to run as root, so initdb and pg_ctl run as that user
#
# Environment:
#   PG_BIN         directory with the PostgreSQL binaries, searched before PATH
#   PGO_DURATION   seconds per pgbench run (default: 8)
#   LLVM_PROFDATA  llvm-profdata to use instead of the one from llvm-tools
#   PG_OS_USER     user that runs initdb and pg_ctl when the script runs as
#                  root (default: postgres)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT="$ROOT/pgo/pg_doorman.profdata.gz"
DURATION="${PGO_DURATION:-8}"
PG_OS_USER="${PG_OS_USER:-postgres}"

target_dir="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$target_dir" in
    /*) ;;
    *) target_dir="$PWD/$target_dir" ;;
esac
GEN_DIR="$target_dir/pgo-gen"

# pgbench talks to pg_doorman over plain TCP.
export PGSSLMODE=disable PGGSSENCMODE=disable

WORK=""
PGDATA_DIR=""
DOORMAN_PID=""
DOORMAN_PORT=""
PG_PORT=""
BIN=""
PROFDATA=""
HOST=""
RUSTC_VERSION=""
RUSTC_LLVM=""
PKG_VERSION=""

log() { printf '==> %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '3,/^[^#]/{/^#/s/^# \{0,1\}//p;}' "${BASH_SOURCE[0]}"
}

show_tail() {
    if [ -s "$1" ]; then
        printf -- '--- last lines of %s\n' "${1##*/}" >&2
        tail -n 20 "$1" >&2
    fi
}

# Sends SIGTERM to pg_doorman, waits up to 30 s for it to exit (SIGKILL
# after that) and returns its exit status.
stop_doorman() {
    local pid="$DOORMAN_PID" ticks=0 status=0
    [ -n "$pid" ] || return 0
    DOORMAN_PID=""
    kill -TERM "$pid" 2>/dev/null || true
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$ticks" -ge 300 ]; then
            kill -KILL "$pid" 2>/dev/null || true
            break
        fi
        sleep 0.1
        ticks=$((ticks + 1))
    done
    wait "$pid" 2>/dev/null || status=$?
    return "$status"
}

# The PostgreSQL server refuses to run as root; a root run starts it as
# PG_OS_USER.
as_pg() {
    if [ "$(id -u)" -eq 0 ]; then
        runuser -u "$PG_OS_USER" -- "$@"
    else
        "$@"
    fi
}

cleanup() {
    local status=$?
    set +e
    stop_doorman
    if [ -n "$PGDATA_DIR" ] && [ -f "$PGDATA_DIR/postmaster.pid" ]; then
        as_pg pg_ctl -D "$PGDATA_DIR" -m immediate -w stop >/dev/null 2>&1
    fi
    if [ "$status" -ne 0 ] && [ -n "$WORK" ]; then
        show_tail "$WORK/pg_doorman.log"
        show_tail "$WORK/postgres.log"
    fi
    if [ -n "$WORK" ]; then
        rm -rf "$WORK"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Prints a TCP port on 127.0.0.1 that nothing listens on. The range stays
# below the ephemeral ports of Linux and macOS.
free_port() {
    local port attempt=0
    while [ "$attempt" -lt 50 ]; do
        port=$((20000 + RANDOM % 12000))
        if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            printf '%s\n' "$port"
            return 0
        fi
        attempt=$((attempt + 1))
    done
    die "no free TCP port found on 127.0.0.1"
}

count_profraw() {
    local n=0 f
    for f in "$WORK"/profraw/*.profraw; do
        if [ -e "$f" ]; then
            n=$((n + 1))
        fi
    done
    printf '%s\n' "$n"
}

size_mb() {
    LC_ALL=C awk -v bytes="$(wc -c <"$1")" 'BEGIN { printf "%.1f MB", bytes / 1048576 }'
}

preflight() {
    local tool version_text profdata_llvm

    if [ "$(id -u)" -eq 0 ]; then
        command -v runuser >/dev/null 2>&1 ||
            die "running as root needs runuser to start PostgreSQL as PG_OS_USER"
        id -u "$PG_OS_USER" >/dev/null 2>&1 ||
            die "PG_OS_USER=$PG_OS_USER does not exist; PostgreSQL refuses to run as root"
        [ "$(id -u "$PG_OS_USER")" -ne 0 ] || die "PG_OS_USER must not be root"
    fi

    case "$DURATION" in
        '' | *[!0-9]*) die "PGO_DURATION must be a number of seconds, got '$DURATION'" ;;
    esac
    [ "$DURATION" -gt 0 ] || die "PGO_DURATION must be positive"

    if [ -n "${PG_BIN:-}" ]; then
        [ -d "$PG_BIN" ] || die "PG_BIN=$PG_BIN is not a directory"
        PATH="$PG_BIN:$PATH"
    fi
    for tool in initdb pg_ctl createdb pgbench; do
        command -v "$tool" >/dev/null 2>&1 ||
            die "$tool not found in ${PG_BIN:+PG_BIN or }PATH. Install PostgreSQL" \
                "and pgbench, and point PG_BIN to the directory with initdb and" \
                "pg_ctl when they are not in PATH (Debian and Ubuntu:" \
                "PG_BIN=/usr/lib/postgresql/<version>/bin)"
    done
    for tool in cargo rustc gzip; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool not found in PATH"
    done

    # rustc runs from the repository root, so rust-toolchain.toml applies.
    version_text="$(rustc -vV)"
    RUSTC_VERSION="$(printf '%s\n' "$version_text" | sed -n 's/^release: //p')"
    HOST="$(printf '%s\n' "$version_text" | sed -n 's/^host: //p')"
    RUSTC_LLVM="$(printf '%s\n' "$version_text" | sed -n 's/^LLVM version: //p')"

    if [ -n "${LLVM_PROFDATA:-}" ]; then
        PROFDATA="$LLVM_PROFDATA"
        [ -x "$PROFDATA" ] || die "LLVM_PROFDATA=$PROFDATA is not an executable file"
    else
        PROFDATA="$(rustc --print sysroot)/lib/rustlib/$HOST/bin/llvm-profdata"
        [ -x "$PROFDATA" ] ||
            die "llvm-profdata not found at $PROFDATA. Install the llvm-tools" \
                "component: rustup component add llvm-tools --toolchain $RUSTC_VERSION"
    fi
    # rustc cannot read a profile written by a newer LLVM.
    version_text="$("$PROFDATA" --version 2>&1)"
    profdata_llvm="$(printf '%s\n' "$version_text" | sed -n 's/.*LLVM version \([0-9][0-9]*\).*/\1/p')"
    [ "$profdata_llvm" = "${RUSTC_LLVM%%.*}" ] ||
        die "$PROFDATA is LLVM ${profdata_llvm:-of unknown version}, rustc $RUSTC_VERSION" \
            "uses LLVM $RUSTC_LLVM"

    PKG_VERSION="$(sed -n '/^version = "/{s/^version = "\(.*\)"$/\1/p;q;}' "$ROOT/Cargo.toml")"
}

warn_foreign_host() {
    if [ "$HOST" != "x86_64-unknown-linux-gnu" ]; then
        warn "this host is $HOST. Release builds run on x86_64-unknown-linux-gnu" \
            "and find no symbols of their own in a profile recorded here, so" \
            "commit only a profile recorded on x86_64 Linux."
    fi
}

build_instrumented() {
    if [ -n "${RUSTFLAGS:-}" ]; then
        warn "RUSTFLAGS='$RUSTFLAGS' applies to the instrumented build too." \
            "Release builds run without it, and functions it changes lose PGO."
    fi
    log "Building instrumented pg_doorman in ${GEN_DIR#"$ROOT"/}"
    # Build scripts and proc macros get the flag as well and drop their own
    # profiles into profraw-build, which is never merged. The path stays the
    # same between runs, so cargo reuses the instrumented build.
    rm -rf "$GEN_DIR/profraw-build"
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-Cprofile-generate=$GEN_DIR/profraw-build" \
        cargo build --release --bin pg_doorman --target-dir "$GEN_DIR"
    BIN="$GEN_DIR/release/pg_doorman"
    [ -x "$BIN" ] || die "instrumented binary not found at $BIN"
}

start_postgres() {
    local base="${TMPDIR:-/tmp}" pg_version
    base="${base%/}"
    # Unix socket paths are limited to 103 bytes.
    [ "${#base}" -le 60 ] || base=/tmp
    WORK="$(mktemp -d "$base/pg_doorman-pgo.XXXXXX")"
    mkdir "$WORK/profraw"
    if [ "$(id -u)" -eq 0 ]; then
        chown "$PG_OS_USER" "$WORK"
    fi
    PGDATA_DIR="$WORK/pgdata"
    PG_PORT="$(free_port)"

    pg_version="$(pg_ctl --version)"
    log "Starting PostgreSQL ${pg_version#pg_ctl (PostgreSQL) } in $WORK"
    as_pg initdb -D "$PGDATA_DIR" -U postgres -A trust -E UTF8 --no-locale --no-sync \
        >"$WORK/initdb.log" 2>&1 || {
        cat "$WORK/initdb.log" >&2
        die "initdb failed"
    }
    cat >>"$PGDATA_DIR/postgresql.conf" <<EOF

# Throwaway cluster of scripts/pgo-profile.sh: unix socket only, no fsync.
listen_addresses = ''
port = $PG_PORT
unix_socket_directories = '$WORK'
fsync = off
synchronous_commit = off
full_page_writes = off
EOF
    as_pg pg_ctl -D "$PGDATA_DIR" -l "$WORK/postgres.log" -w -t 60 start >/dev/null ||
        die "PostgreSQL did not start"

    log "Initializing pgbench tables, scale 10"
    createdb -h "$WORK" -p "$PG_PORT" -U postgres bench
    pgbench -i -s 10 -q -h "$WORK" -p "$PG_PORT" -U postgres bench \
        >"$WORK/pgbench-init.log" 2>&1 || {
        cat "$WORK/pgbench-init.log" >&2
        die "pgbench -i failed"
    }
}

write_doorman_config() {
    local release_line="$1"
    cat >"$WORK/pg_doorman.toml" <<EOF
[general]
host = "127.0.0.1"
port = $DOORMAN_PORT
admin_username = "admin"
admin_password = "admin"
pg_hba.content = "host all all 127.0.0.1/32 trust"
worker_threads = 4

[pools.bench]
server_host = "$WORK"
server_port = $PG_PORT
pool_mode = "transaction"
$release_line

[[pools.bench.users]]
username = "postgres"
password = ""
pool_size = 8
EOF
}

wait_for_doorman() {
    local ticks=0
    while ! (exec 3<>"/dev/tcp/127.0.0.1/$DOORMAN_PORT") 2>/dev/null; do
        kill -0 "$DOORMAN_PID" 2>/dev/null || die "pg_doorman exited during startup"
        [ "$ticks" -lt 300 ] ||
            die "pg_doorman does not accept connections on 127.0.0.1:$DOORMAN_PORT after 30 s"
        sleep 0.1
        ticks=$((ticks + 1))
    done
}

# Runs pgbench through pg_doorman and prints the TPS lines of the run.
run_pgbench() {
    local label="$1" out="$WORK/pgbench.log" tps line
    shift
    if ! pgbench -n -h 127.0.0.1 -p "$DOORMAN_PORT" -U postgres -T "$DURATION" "$@" bench \
        >"$out" 2>&1; then
        cat "$out" >&2
        die "pgbench failed: $label"
    fi
    tps="$(grep '^tps = ' "$out" || true)"
    if [ -z "$tps" ]; then
        cat "$out" >&2
        die "no TPS in the pgbench output: $label"
    fi
    printf '%s\n' "$tps" | while IFS= read -r line; do
        printf '    %-35s %s\n' "$label" "$line"
    done
}

train() {
    local title="$1" release_line="$2" before after status=0 mode clients
    log "Training with $title"
    DOORMAN_PORT="$(free_port)"
    write_doorman_config "$release_line"
    before="$(count_profraw)"
    # PG_DOORMAN_CI_SHUTDOWN_ONLY=1 turns a Ctrl-C that reaches pg_doorman
    # through the terminal into a shutdown. Without it pg_doorman, whose
    # stdin is not a terminal, answers SIGINT with a binary upgrade and
    # leaves a new process behind.
    LLVM_PROFILE_FILE="$WORK/profraw/pg_doorman-%p-%m.profraw" PG_DOORMAN_CI_SHUTDOWN_ONLY=1 \
        "$BIN" "$WORK/pg_doorman.toml" >"$WORK/pg_doorman.log" 2>&1 &
    DOORMAN_PID=$!
    wait_for_doorman
    for mode in simple extended prepared; do
        for clients in 8 32; do
            run_pgbench "select-only, $mode, $clients clients:" \
                -S -M "$mode" -c "$clients" -j "$clients"
        done
    done
    run_pgbench "tpcb-like, simple, 32 clients:" -c 32 -j 32
    stop_doorman || status=$?
    [ "$status" -eq 0 ] || die "pg_doorman exited with status $status after SIGTERM"
    after="$(count_profraw)"
    [ "$after" -gt "$before" ] ||
        die "pg_doorman exited without writing a raw profile into $WORK/profraw"
}

merge_profile() {
    local merged="$WORK/pg_doorman.profdata" tmp="$OUTPUT.tmp.$$" summary
    log "Merging $(count_profraw) raw profiles"
    "$PROFDATA" merge -o "$merged" "$WORK"/profraw/*.profraw
    summary="$("$PROFDATA" show "$merged")"
    printf '%s\n' "$summary" | grep -E '^(Total functions|Maximum function count):' |
        sed 's/^/    /' || true

    mkdir -p "${OUTPUT%/*}"
    gzip -9 -n -c "$merged" >"$tmp" || {
        rm -f "$tmp"
        die "gzip failed"
    }
    mv -f "$tmp" "$OUTPUT"
    log "Wrote ${OUTPUT#"$ROOT"/}: $(size_mb "$OUTPUT"), $(size_mb "$merged") unpacked"
    log "It matches release builds of pg_doorman $PKG_VERSION made by rustc $RUSTC_VERSION on $HOST"
}

main() {
    case "${1:-}" in
        '') ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown argument '$1', see --help" ;;
    esac

    cd "$ROOT"
    preflight
    log "pg_doorman $PKG_VERSION, rustc $RUSTC_VERSION (LLVM $RUSTC_LLVM), host $HOST"
    warn_foreign_host
    build_instrumented
    start_postgres
    train "the default release_query" ""
    train 'release_query = ""' 'release_query = ""'
    merge_profile
    warn_foreign_host
}

main "$@"
