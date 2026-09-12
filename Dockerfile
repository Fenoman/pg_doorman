FROM rust:1.88.0-slim-trixie AS builder

RUN apt-get update && \
    apt-get install -y build-essential pkg-config libssl-dev perl

COPY . /app
WORKDIR /app
RUN cargo build --release

# The runtime stage is distroless and has no shell, so everything that used to
# be an in-image `RUN` has to be materialised here and copied in as files.
#
# Trivy on the old debian:*-slim runtime reported ~248 vulnerabilities
# (77 HIGH/CRITICAL), none of them fixable: `apt-get upgrade` had already
# pulled everything the security stream offers, and the remainder are
# `will_not_fix`/`affected` entries against packages we never call —
# postgresql-client alone accounted for +72 CVEs (24 HIGH/CRITICAL), and it
# was only ever in the image for manual `psql` debugging. distroless/cc keeps
# libc6, libssl3t64, libgcc-s1, zlib1g, libzstd1 and ca-certificates, which is
# exactly the closure `ldd` reports for both binaries, and nothing else.
COPY --from=gcr.io/distroless/cc-debian13:latest /etc/passwd /distroless/passwd
COPY --from=gcr.io/distroless/cc-debian13:latest /etc/group /distroless/group

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

FROM gcr.io/distroless/cc-debian13:latest

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
