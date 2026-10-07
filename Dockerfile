FROM gcr.io/distroless/cc-debian13@sha256:159783207c2cd44c2aa5715961d13c8612368ac9bd450f887e3f08fc8ea461e3 AS runtime-base

FROM rust:1.88.0-slim-trixie AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev perl

# Embed the resolved Rust dependency inventory so image scanners cover both
# application binaries as well as the operating-system packages.
RUN cargo install cargo-auditable --version 0.7.5 --locked

COPY . /app
WORKDIR /app
# scripts/pgo-rustflags.sh adds the committed PGO profile to RUSTFLAGS when
# pgo/pg_doorman.profdata.gz is present. `--build-arg PGO=0` builds without it.
#
# cargo-auditable runs as RUSTC_WRAPPER instead of through `cargo auditable
# build`, which sets RUSTC_WORKSPACE_WRAPPER: cargo mixes the path of the
# workspace wrapper into the symbol names of the pg_doorman crates, and the
# profile, recorded by plain cargo, would then match only the dependencies.
# RUSTC_WRAPPER leaves symbol names alone. CARGO_AUDITABLE_ORIG_ARGS is what
# `cargo auditable build --locked` hands to its wrapper (cargo-auditable 0.7.5).
# The build fails when a binary comes out without the dependency inventory.
ARG PGO=1
RUN pgo_flags="$(PGO="$PGO" bash scripts/pgo-rustflags.sh)" && \
    if [ -n "$pgo_flags" ]; then \
        export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$pgo_flags"; \
    fi && \
    echo "RUSTFLAGS=${RUSTFLAGS:-}" && \
    CARGO_AUDITABLE_ORIG_ARGS='{"offline":false,"locked":true,"frozen":false,"config":[]}' \
    RUSTC_WRAPPER=cargo-auditable \
    cargo build --locked --release --bin pg_doorman --bin patroni_proxy && \
    for bin in pg_doorman patroni_proxy; do \
        readelf -SW "target/release/$bin" | grep -qE '[[:space:]]\.dep-v0[[:space:]]' || \
            { echo "target/release/$bin has no dependency inventory" >&2; exit 1; }; \
    done

# The runtime stage is distroless and has no shell, so everything that used to
# be an in-image `RUN` has to be materialised here and copied in as files.
COPY --from=runtime-base /etc/passwd /distroless/passwd
COPY --from=runtime-base /etc/group /distroless/group

# Keep uid/gid 999 rather than adopting distroless' own `nonroot` (65532):
# operators already mount read-only configs owned by 999, and the daemon-mode
# `daemon.user`/`daemon.group` lookup goes through getpwnam(3), so the
# account has to exist by name as well as by number. The distroless entries
# are preserved so `root`/`nobody`/`nonroot` keep resolving.
RUN cp /distroless/passwd /rootfs-passwd && \
    cp /distroless/group /rootfs-group && \
    echo 'pgdoorman:x:999:999::/nonexistent:/sbin/nologin' >> /rootfs-passwd && \
    echo 'pgdoorman:x:999:' >> /rootfs-group && \
    install -d -m 0755 -o 999 -g 999 /rootfs-etc-pg_doorman

FROM runtime-base

COPY --from=builder /rootfs-passwd /etc/passwd
COPY --from=builder /rootfs-group /etc/group
COPY --from=builder --chown=999:999 /rootfs-etc-pg_doorman /etc/pg_doorman

COPY --from=builder /app/target/release/pg_doorman /usr/bin/pg_doorman
COPY --from=builder /app/target/release/patroni_proxy /usr/bin/patroni_proxy
WORKDIR /etc/pg_doorman
# Run as non-root. Port 6432 does not require CAP_NET_BIND_SERVICE.
# The SIGUSR2 binary-upgrade fd-passing path operates entirely within the
# process and does not need root. Numeric form so the USER directive keeps
# satisfying `runAsNonRoot` even if /etc/passwd is replaced at deploy time.
USER 999:999
ENV RUST_LOG=info
CMD ["pg_doorman"]
# SIGTERM for immediate shutdown in containers.
# SIGINT in non-TTY triggers binary upgrade (spawns child, PID 1 exits,
# container dies). SIGTERM avoids this.
STOPSIGNAL SIGTERM
