#!/usr/bin/env bash
#
# Prints the RUSTFLAGS that make a release build of pg_doorman use the
# committed PGO profile, pgo/pg_doorman.profdata.gz. Prints nothing when
# the profile is absent or PGO=0 is set, and the build then runs as
# without PGO.
#
# Usage:
#   flags="$(scripts/pgo-rustflags.sh [TARGET_DIR])"
#   if [ -n "$flags" ]; then export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$flags"; fi
#
# The profile is unpacked into TARGET_DIR/pgo (default: $CARGO_TARGET_DIR,
# else target/ of the repository) as pg_doorman-<checksum>.profdata. The
# checksum ties the name to the profile contents: another profile changes
# RUSTFLAGS and cargo rebuilds with it, while builds with the same profile
# reuse the unpacked file and stay incremental. Needs only bash, cksum and
# gunzip.
#
# A stale profile is safe: functions that changed since it was recorded
# get no profile data and are compiled as without PGO, and
# -no-pgo-warn-mismatch keeps LLVM from warning about each of them.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile="$root/pgo/pg_doorman.profdata.gz"

if [ "${PGO:-1}" = 0 ]; then
    echo "pgo-rustflags: PGO=0, building without the PGO profile" >&2
    exit 0
fi
if [ ! -f "$profile" ]; then
    echo "pgo-rustflags: no pgo/pg_doorman.profdata.gz, building without PGO" >&2
    exit 0
fi

target_dir="${1:-${CARGO_TARGET_DIR:-$root/target}}"
case "$target_dir" in
    /*) ;;
    *) target_dir="$PWD/$target_dir" ;;
esac

sum="$(cksum <"$profile")"
unpacked="$target_dir/pgo/pg_doorman-${sum%% *}.profdata"
case "$unpacked" in
    *[[:space:]]*)
        echo "pgo-rustflags: RUSTFLAGS cannot carry the path '$unpacked'." \
            "Use a target directory without whitespace, or PGO=0." >&2
        exit 1
        ;;
esac

if [ ! -s "$unpacked" ]; then
    mkdir -p "${unpacked%/*}"
    gunzip -c "$profile" >"$unpacked.tmp.$$" || {
        rm -f "$unpacked.tmp.$$"
        exit 1
    }
    mv -f "$unpacked.tmp.$$" "$unpacked"
fi

echo "pgo-rustflags: building with pgo/pg_doorman.profdata.gz" >&2
printf '%s\n' "-Cprofile-use=$unpacked -Cllvm-args=-no-pgo-warn-mismatch"
